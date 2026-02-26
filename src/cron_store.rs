use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde_json::json;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CronStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CronJobInput {
    pub name: Option<String>,
    pub schedule_kind: String,
    pub schedule_json: String,
    pub payload_kind: String,
    pub payload_json: String,
    pub session_target: String,
    pub next_run_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CronJobRow {
    pub id: String,
    pub name: Option<String>,
    pub schedule_kind: String,
    pub payload_kind: String,
    pub session_target: String,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CronRunRow {
    pub run_id: String,
    pub job_id: String,
    pub status: String,
    pub trigger_source: String,
    pub created_at: String,
}

impl CronStore {
    pub fn init(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS cron_jobs (
                id TEXT PRIMARY KEY,
                name TEXT,
                schedule_kind TEXT NOT NULL CHECK (schedule_kind IN ('at', 'every', 'cron')),
                schedule_json TEXT NOT NULL,
                payload_kind TEXT NOT NULL CHECK (payload_kind IN ('systemEvent', 'agentTurn')),
                payload_json TEXT NOT NULL,
                session_target TEXT NOT NULL CHECK (session_target IN ('main', 'isolated')),
                delivery_json TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                next_run_at TEXT,
                last_run_at TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE INDEX IF NOT EXISTS idx_cron_jobs_enabled_next_run
            ON cron_jobs(enabled, next_run_at);

            CREATE TABLE IF NOT EXISTS cron_job_runs (
                run_id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'timed_out', 'cancelled')),
                trigger_source TEXT NOT NULL DEFAULT 'schedule' CHECK (trigger_source IN ('schedule', 'manual', 'retry')),
                started_at TEXT,
                finished_at TEXT,
                duration_ms INTEGER,
                error_text TEXT,
                output_json TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                FOREIGN KEY(job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_cron_runs_job_created
            ON cron_job_runs(job_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS cron_scheduler_state (
                key TEXT PRIMARY KEY,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            "#,
        )?;

        Ok(Self { db_path })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn add_job(&self, input: CronJobInput) -> Result<String> {
        validate_job(&input)?;

        let conn = Connection::open(&self.db_path)?;
        let id = Uuid::new_v4().to_string();

        conn.execute(
            r#"
            INSERT INTO cron_jobs(
                id, name, schedule_kind, schedule_json,
                payload_kind, payload_json, session_target,
                delivery_json, enabled, next_run_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 1, ?8)
            "#,
            params![
                id,
                input.name,
                input.schedule_kind,
                input.schedule_json,
                input.payload_kind,
                input.payload_json,
                input.session_target,
                input.next_run_at,
            ],
        )?;

        Ok(id)
    }

    pub fn list_jobs(&self, limit: usize) -> Result<Vec<CronJobRow>> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, name, schedule_kind, payload_kind, session_target, enabled, next_run_at, last_run_at
            FROM cron_jobs
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(CronJobRow {
                id: row.get(0)?,
                name: row.get(1)?,
                schedule_kind: row.get(2)?,
                payload_kind: row.get(3)?,
                session_target: row.get(4)?,
                enabled: row.get::<_, i64>(5)? == 1,
                next_run_at: row.get(6)?,
                last_run_at: row.get(7)?,
            })
        })?;

        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn trigger_run_manual(&self, job_id: &str) -> Result<String> {
        let conn = Connection::open(&self.db_path)?;

        let exists: i64 = conn.query_row(
            "SELECT COUNT(1) FROM cron_jobs WHERE id = ?1",
            params![job_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(anyhow!("job introuvable: {job_id}"));
        }

        let run_id = Uuid::new_v4().to_string();
        conn.execute(
            r#"
            INSERT INTO cron_job_runs(run_id, job_id, status, trigger_source, started_at, output_json)
            VALUES (?1, ?2, 'queued', 'manual', ?3, ?4)
            "#,
            params![
                run_id,
                job_id,
                Utc::now().to_rfc3339(),
                json!({"note":"queued for scheduler"}).to_string()
            ],
        )?;

        Ok(run_id)
    }

    pub fn list_runs(&self, job_id: &str, limit: usize) -> Result<Vec<CronRunRow>> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = conn.prepare(
            r#"
            SELECT run_id, job_id, status, trigger_source, created_at
            FROM cron_job_runs
            WHERE job_id = ?1
            ORDER BY created_at DESC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![job_id, limit as i64], |row| {
            Ok(CronRunRow {
                run_id: row.get(0)?,
                job_id: row.get(1)?,
                status: row.get(2)?,
                trigger_source: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn validate_job(input: &CronJobInput) -> Result<()> {
    if !matches!(input.schedule_kind.as_str(), "at" | "every" | "cron") {
        return Err(anyhow!("schedule_kind invalide (at|every|cron)"));
    }
    if !matches!(input.payload_kind.as_str(), "systemEvent" | "agentTurn") {
        return Err(anyhow!("payload_kind invalide (systemEvent|agentTurn)"));
    }
    if !matches!(input.session_target.as_str(), "main" | "isolated") {
        return Err(anyhow!("session_target invalide (main|isolated)"));
    }
    if input.schedule_json.trim().is_empty() || input.payload_json.trim().is_empty() {
        return Err(anyhow!("schedule_json/payload_json ne doivent pas être vides"));
    }

    if input.session_target == "main" && input.payload_kind != "systemEvent" {
        return Err(anyhow!("main requiert payload_kind=systemEvent"));
    }
    if input.session_target == "isolated" && input.payload_kind != "agentTurn" {
        return Err(anyhow!("isolated requiert payload_kind=agentTurn"));
    }

    Ok(())
}
