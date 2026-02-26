use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CronStore {
    db_path: PathBuf,
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
}
