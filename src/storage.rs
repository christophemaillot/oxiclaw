use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use log::debug;

use crate::memory;

#[derive(Debug, Clone)]
pub struct TranscriptStore {
    basedir: PathBuf,
    file: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
struct TranscriptLine {
    ts: String,
    session_id: String,
    role: String,
    content: String,
}

impl TranscriptStore {
    pub fn new(file: PathBuf, _index_file: PathBuf) -> Result<Self> {
        let basedir = file
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("chemin transcripts invalide"))?
            .to_path_buf();

        let store = Self { basedir, file };
        debug!("indexer: catch-up au démarrage basedir={}", store.basedir.display());
        memory::run_indexer_once(&store.basedir)?; // catch-up au démarrage
        Ok(store)
    }

    pub fn append(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        let line = TranscriptLine {
            ts: Utc::now().to_rfc3339(),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
        };

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;

        let json_line = serde_json::to_string(&line)?;
        writeln!(f, "{json_line}")?;

        debug!("indexer: trigger append role={} file={}", role, self.file.display());
        memory::run_indexer_once(&self.basedir)?;
        Ok(())
    }
}
