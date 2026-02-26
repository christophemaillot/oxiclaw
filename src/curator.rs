use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use log::{debug, info};

use crate::llm::openai_compat::OpenAiCompatClient;
use crate::memory;
use crate::llm::{LlmClient, LlmRequest};
use crate::session::ChatMessage;

#[derive(Debug, Deserialize)]
struct CuratorConfig {
    enabled: Option<bool>,
    llm: Option<CuratorLlm>,
    window: Option<CuratorWindow>,
    apply: Option<CuratorApply>,
}

#[derive(Debug, Deserialize)]
struct CuratorLlm {
    base_url: Option<String>,
    model: Option<String>,
    api_key_env: Option<String>,
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct CuratorWindow {
    max_lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CuratorApply {
    dry_run: Option<bool>,
    max_additions_per_run: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct CuratorState {
    last_file: Option<String>,
    last_line: usize,
    last_run_at: String,
}

#[derive(Debug, Deserialize, Default)]
struct CuratorOutput {
    #[serde(default)]
    memory_additions: Vec<String>,
    #[serde(default)]
    agent_additions: Vec<String>,
    #[serde(default)]
    user_additions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TranscriptLine {
    role: String,
    content: String,
}

pub async fn run_manual(basedir: &PathBuf, default_endpoint: &str, default_model: &str, default_api_key: &str) -> Result<String> {
    info!("curator:start basedir={}", basedir.display());
    let cfg = read_config(basedir)?;
    if cfg.enabled == Some(false) {
        return Ok("curator: désactivé (conf/curator.toml)".to_string());
    }

    let max_lines = cfg.window.and_then(|w| w.max_lines).unwrap_or(800).max(50);
    let dry_run = cfg.apply.as_ref().and_then(|a| a.dry_run).unwrap_or(false);
    let max_additions = cfg
        .apply
        .as_ref()
        .and_then(|a| a.max_additions_per_run)
        .unwrap_or(10)
        .max(1);

    let mut state = read_state(basedir)?;
    let snippets = collect_transcript_window(basedir, &state, max_lines)?;
    debug!("curator:window lines={} max_lines={}", snippets.len(), max_lines);
    if snippets.is_empty() {
        info!("curator:skip no_new_data");
        return Ok("curator: rien de nouveau à curer".to_string());
    }

    let endpoint = cfg
        .llm
        .as_ref()
        .and_then(|l| l.base_url.clone())
        .map(to_chat_completions_endpoint)
        .unwrap_or_else(|| default_endpoint.to_string());

    let model = cfg
        .llm
        .as_ref()
        .and_then(|l| l.model.clone())
        .unwrap_or_else(|| default_model.to_string());

    let api_key = cfg
        .llm
        .as_ref()
        .and_then(|l| l.api_key_env.clone())
        .and_then(|name| env::var(name).ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default_api_key.to_string());

    let temperature = cfg
        .llm
        .as_ref()
        .and_then(|l| l.temperature)
        .unwrap_or(0.1);

    let client = OpenAiCompatClient::new(endpoint, api_key);

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: curator_system_prompt(max_additions, basedir),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!(
                "Voici les nouveaux transcripts à curer. Extrait uniquement des faits stables et utiles.\n\n{}",
                snippets.join("\n")
            ),
        },
    ];

    let raw = client
        .complete(LlmRequest {
            model,
            messages,
            temperature,
        })
        .await?;

    let out = parse_curator_output(&raw)?;

    let mut applied_memory = 0usize;
    let mut applied_agent = 0usize;
    let mut applied_user = 0usize;

    if !dry_run {
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let memory_path = basedir.join("memory").join(format!("MEMORY-{day}.md"));
        applied_memory = apply_lines(&memory_path, out.memory_additions.iter().take(max_additions))?;
        applied_agent = apply_lines(&basedir.join("AGENT.md"), out.agent_additions.iter().take(max_additions))?;
        applied_user = apply_lines(&basedir.join("USER.md"), out.user_additions.iter().take(max_additions))?;

        if applied_memory > 0 || applied_agent > 0 || applied_user > 0 {
            info!("curator:trigger indexer after memory updates");
            memory::run_indexer_once(basedir)?;
        }
    }

    if let Some((file, line)) = latest_cursor(basedir)? {
        state.last_file = Some(file);
        state.last_line = line;
    }
    state.last_run_at = Utc::now().to_rfc3339();
    write_state(basedir, &state)?;

    let msg = if dry_run {
        format!(
            "curator dry-run: {} propositions MEMORY, {} AGENT, {} USER",
            out.memory_additions.len(),
            out.agent_additions.len(),
            out.user_additions.len()
        )
    } else {
        format!(
            "curator ok: +{} MEMORY, +{} AGENT, +{} USER (propositions: {}/{}/{})",
            applied_memory,
            applied_agent,
            applied_user,
            out.memory_additions.len(),
            out.agent_additions.len(),
            out.user_additions.len()
        )
    };

    info!(
        "curator:done dry_run={} applied_memory={} applied_agent={} applied_user={}",
        dry_run, applied_memory, applied_agent, applied_user
    );

    Ok(msg)
}

fn read_config(basedir: &Path) -> Result<CuratorConfig> {
    let p = basedir.join("conf").join("curator.toml");
    if !p.exists() {
        ensure_default_config(&p)?;
    }
    let raw = fs::read_to_string(p)?;
    let cfg: CuratorConfig = toml::from_str(&raw)?;
    Ok(cfg)
}

fn ensure_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(
            path,
            r#"enabled = true

[llm]
# base_url = "https://api.openai.com/v1"
# model = "openai-codex/gpt-5.3-codex"
# api_key_env = "CURATOR_API_KEY"
temperature = 0.1

[window]
max_lines = 800

[apply]
dry_run = false
max_additions_per_run = 10
"#,
        )?;
    }
    Ok(())
}

fn state_path(basedir: &Path) -> PathBuf {
    basedir.join("state").join("curator_state.json")
}

fn read_state(basedir: &Path) -> Result<CuratorState> {
    let p = state_path(basedir);
    if !p.exists() {
        return Ok(CuratorState::default());
    }
    let raw = fs::read_to_string(p)?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn write_state(basedir: &Path, state: &CuratorState) -> Result<()> {
    fs::write(state_path(basedir), serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn list_transcript_files(basedir: &Path) -> Result<Vec<PathBuf>> {
    let dir = basedir.join("transcripts");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with("session-") && name.ends_with(".jsonl") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn collect_transcript_window(basedir: &Path, state: &CuratorState, max_lines: usize) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    for path in list_transcript_files(basedir)? {
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };

        let start_line = if state.last_file.as_deref() == Some(&file_name) {
            state.last_line + 1
        } else if state
            .last_file
            .as_ref()
            .map(|f| f.as_str() < file_name.as_str())
            .unwrap_or(true)
        {
            1
        } else {
            continue;
        };

        let f = File::open(&path)?;
        let reader = BufReader::new(f);
        for (i, line) in reader.lines().enumerate() {
            let line_no = i + 1;
            if line_no < start_line {
                continue;
            }
            let raw = match line {
                Ok(v) => v,
                Err(_) => continue,
            };
            let p: TranscriptLine = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            rows.push(format!("[{}:{}] [{}] {}", file_name, line_no, p.role, p.content));
            if rows.len() >= max_lines {
                return Ok(rows);
            }
        }
    }
    Ok(rows)
}

fn latest_cursor(basedir: &Path) -> Result<Option<(String, usize)>> {
    let mut latest: Option<(String, usize)> = None;
    for path in list_transcript_files(basedir)? {
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };
        let f = File::open(&path)?;
        let reader = BufReader::new(f);
        let mut n = 0usize;
        for line in reader.lines() {
            if line.is_ok() {
                n += 1;
            }
        }
        latest = Some((file_name, n));
    }
    Ok(latest)
}

fn apply_lines<'a>(path: &Path, lines: impl Iterator<Item = &'a String>) -> Result<usize> {
    let mut existing = fs::read_to_string(path).unwrap_or_default();
    let mut added = 0usize;

    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    for line in lines {
        let line = sanitize_line(line);
        if line.is_empty() || line.len() > 500 {
            continue;
        }
        if contains_sensitive(&line) {
            continue;
        }
        if existing.lines().any(|l| l.contains(&line)) {
            continue;
        }
        writeln!(f, "- [{}] {}", Utc::now().to_rfc3339(), line)?;
        existing.push_str(&format!("\n{line}"));
        added += 1;
    }
    Ok(added)
}

fn contains_sensitive(s: &str) -> bool {
    let lower = s.to_lowercase();
    ["token", "password", "api_key", "secret", "bearer ", "ssh-"]
        .iter()
        .any(|w| lower.contains(w))
}

fn sanitize_line(s: &str) -> String {
    s.trim().trim_start_matches('-').trim().replace('\n', " ")
}

fn parse_curator_output(raw: &str) -> Result<CuratorOutput> {
    if let Ok(v) = serde_json::from_str::<CuratorOutput>(raw.trim()) {
        return Ok(v);
    }

    let start = raw.find('{').ok_or_else(|| anyhow::anyhow!("curator: JSON introuvable"))?;
    let end = raw.rfind('}').ok_or_else(|| anyhow::anyhow!("curator: JSON introuvable"))?;
    let slice = &raw[start..=end];
    let v: CuratorOutput = serde_json::from_str(slice)?;
    Ok(v)
}

fn curator_system_prompt(max_additions: usize, basedir: &Path) -> String {
    let template = fs::read_to_string(
        basedir
            .join("conf")
            .join("prompts")
            .join("curator_system.md"),
    )
    .unwrap_or_else(|_| default_curator_prompt().to_string());

    template.replace("__MAX_ADDITIONS__", &max_additions.to_string())
}

fn default_curator_prompt() -> &'static str {
    "Tu es un curator mémoire. Objectif principal: produire des lignes pour la mémoire quotidienne (memory/MEMORY-YYYY-MM-DD.md).\nTu peux aussi proposer quelques lignes AGENT/USER si c'est vraiment stable et structurel.\nNe pas inclure de secrets (tokens, passwords, clés).\nRéponds UNIQUEMENT en JSON strict avec ce format:\n{\"memory_additions\":[\"...\"],\"agent_additions\":[\"...\"],\"user_additions\":[\"...\"]}\nContraintes: max __MAX_ADDITIONS__ lignes par tableau, lignes courtes et concrètes."
}


fn to_chat_completions_endpoint(base_url: String) -> String {
    let b = base_url.trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{}/chat/completions", b)
    } else {
        format!("{}/v1/chat/completions", b)
    }
}
