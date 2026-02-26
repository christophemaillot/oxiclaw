use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub api_key: String,
    pub model: String,
    pub endpoint: String,
    pub basedir: PathBuf,
    pub conf_file: PathBuf,
    pub transcripts_file: PathBuf,
    pub memory_index_file: PathBuf,
    pub http_host: String,
    pub http_port: u16,
    pub telegram_enabled: bool,
    pub telegram_bot_token: Option<String>,
    pub telegram_default_chat_id: Option<String>,
    pub max_turn_steps: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FileConfig {
    llm: Option<LlmConfig>,
    http: Option<HttpConfig>,
    telegram: Option<TelegramConfig>,
    runtime: Option<RuntimeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LlmConfig {
    api_key: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpConfig {
    host: String,
    port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8787,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TelegramConfig {
    enabled: bool,
    bot_token: Option<String>,
    default_chat_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeConfig {
    max_turn_steps: Option<usize>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_turn_steps: Some(10),
        }
    }
}

impl AppConfig {
    pub fn from_env_and_args() -> Result<Self> {
        let basedir = parse_basedir_arg()
            .or_else(|| env::var("OXICLAW_BASEDIR").or_else(|_| env::var("GRIFFE_BASEDIR")).ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("runtime"));

        init_layout(&basedir)?;

        let conf_file = basedir.join("conf").join("config.toml");
        let file_cfg = read_file_config(&conf_file)?;

        let api_key = env::var("OPENAI_API_KEY")
            .or_else(|_| env::var("OPENCLAW_GATEWAY_TOKEN"))
            .ok()
            .or_else(|| file_cfg.llm.as_ref().and_then(|l| l.api_key.clone()))
            .context("OPENAI_API_KEY manquante (ou OPENCLAW_GATEWAY_TOKEN, ou conf/config.toml.llm.api_key)")?;

        let model = env::var("OPENAI_MODEL")
            .or_else(|_| env::var("OPENCLAW_MODEL"))
            .ok()
            .or_else(|| file_cfg.llm.as_ref().and_then(|l| l.model.clone()))
            .unwrap_or_else(|| "openai-codex/gpt-5.3-codex".to_string());

        let base_url = env::var("OPENAI_BASE_URL")
            .or_else(|_| env::var("OPENCLAW_BASE_URL"))
            .ok()
            .or_else(|| file_cfg.llm.as_ref().and_then(|l| l.base_url.clone()))
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        let endpoint = if base_url.trim_end_matches('/').ends_with("/v1") {
            format!("{}/chat/completions", base_url.trim_end_matches('/'))
        } else {
            format!("{}/v1/chat/completions", base_url.trim_end_matches('/'))
        };

        let http_cfg = file_cfg.http.unwrap_or_default();
        let telegram_cfg = file_cfg.telegram.unwrap_or_default();

        let day = Utc::now().format("%Y-%m-%d").to_string();
        let transcripts_file = basedir
            .join("transcripts")
            .join(format!("session-{day}.jsonl"));
        let memory_index_file = basedir.join("memory").join(format!("index-{day}.json"));

        let telegram_enabled = env::var("OXICLAW_TELEGRAM_ENABLED").or_else(|_| env::var("GRIFFE_TELEGRAM_ENABLED"))
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(telegram_cfg.enabled);

        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| telegram_cfg.bot_token.filter(|v| !v.trim().is_empty()));

        let telegram_default_chat_id = env::var("TELEGRAM_DEFAULT_CHAT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| telegram_cfg.default_chat_id.filter(|v| !v.trim().is_empty()));

        let runtime_cfg = file_cfg.runtime.unwrap_or_default();
        let max_turn_steps = runtime_cfg.max_turn_steps.unwrap_or(10).clamp(1, 64);

        Ok(Self {
            api_key,
            model,
            endpoint,
            basedir,
            conf_file,
            transcripts_file,
            memory_index_file,
            http_host: http_cfg.host,
            http_port: http_cfg.port,
            telegram_enabled,
            telegram_bot_token,
            telegram_default_chat_id,
            max_turn_steps,
        })
    }
}

fn parse_basedir_arg() -> Option<PathBuf> {
    let args: Vec<String> = env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--basedir" {
            if let Some(v) = args.get(i + 1) {
                return Some(PathBuf::from(v));
            }
        }
        if let Some(value) = args[i].strip_prefix("--basedir=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn read_file_config(path: &Path) -> Result<FileConfig> {
    let raw = fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let cfg: FileConfig = toml::from_str(&raw).unwrap_or_default();
    Ok(cfg)
}

fn init_layout(basedir: &Path) -> Result<()> {
    fs::create_dir_all(basedir.join("conf"))?;

    fs::create_dir_all(basedir.join("conf").join("prompts"))?;
    fs::create_dir_all(basedir.join("memory"))?;
    fs::create_dir_all(basedir.join("transcripts"))?;
    fs::create_dir_all(basedir.join("state"))?;
    fs::create_dir_all(basedir.join("logs"))?;

    ensure_or_enrich_file(
        &basedir.join("SOUL.md"),
        "# SOUL\n\nTu es OxiClaw.\n\n- Ton: calme, clair, concret.\n- Priorité: aider Christophe à comprendre en profondeur.\n- Méthode: petits pas, test rapide, explication du pourquoi.\n",
    )?;
    ensure_or_enrich_file(
        &basedir.join("IDENTITY.md"),
        "# IDENTITY\n\n- Name: OxiClaw\n- Engine: OxiClaw\n- Stack: Rust\n- Mission: comprendre un agent en le reconstruisant pas à pas.\n",
    )?;
    ensure_or_enrich_file(
        &basedir.join("USER.md"),
        "# USER\n\n- Name: Christophe\n- Language: Français\n- Goal: recoder un agent type OpenClaw pour comprendre son architecture.\n- Preference: explications concrètes + logs debug.\n",
    )?;
    ensure_or_enrich_file(
        &basedir.join("AGENT.md"),
        "# AGENT\n\n## Politique de décision\n- Agir avant de bloquer: utiliser les tools quand nécessaire.\n- Si une info manque, chercher d'abord en mémoire puis poser une question courte.\n- Si une info stable est découverte, utiliser `info_append` pour l'écrire dans AGENT.md ou USER.md.\n\n## Règles\n- SOUL.md est immuable.\n- AGENT.md et USER.md sont append-only via tool.\n",
    )?;

    ensure_file(
        &basedir.join("conf").join("config.toml"),
        r#"[llm]
api_key = ""
model = "kimi-k2-0905-preview"
base_url = "https://api.moonshot.ai/v1"

[http]
host = "127.0.0.1"
port = 8787

[telegram]
enabled = false
bot_token = ""
default_chat_id = ""

[runtime]
max_turn_steps = 10
"#,
    )?;

    ensure_file(
        &basedir.join("conf").join("prompts").join("main_system.md"),
        r#"PROTOCOLE DE SORTIE:
- Si tu as besoin d'un outil, réponds UNIQUEMENT en JSON strict:
  {"type":"tool_call","name":"time","args":{}}
- Si tu as fini, réponds UNIQUEMENT en JSON strict:
  {"type":"final_answer","answer":"..."}
- Quand tu reçois TOOL_RESULT, tu peux soit appeler un autre tool, soit final_answer.
- N'invente jamais un tool absent du catalogue.
- Prise de décision: agir plutôt que bloquer; si une info manque, chercher en mémoire puis poser une seule question courte.
- SOUL.md est immuable: ne jamais tenter de le modifier.
- Si une information stable et utile est apprise, utiliser info_append(target,text) avec target=agent ou user.
"#,
    )?;

    ensure_file(
        &basedir.join("conf").join("prompts").join("curator_system.md"),
        r#"Tu es un curator mémoire. Objectif principal: produire des lignes pour la mémoire quotidienne (memory/MEMORY-YYYY-MM-DD.md).
Tu peux aussi proposer quelques lignes AGENT/USER si c'est vraiment stable et structurel.
Ne pas inclure de secrets (tokens, passwords, clés).
Réponds UNIQUEMENT en JSON strict avec ce format:
{"memory_additions":["..."],"agent_additions":["..."],"user_additions":["..."]}
Contraintes: max __MAX_ADDITIONS__ lignes par tableau, lignes courtes et concrètes.
"#,
    )?;

    ensure_file(
        &basedir.join("conf").join("prompts").join("cron_system.md"),
        r#"Template futur pour le prompt des runs cron (à brancher quand le scheduler interne sera en place)."#,
    )?;

    Ok(())
}

fn ensure_file(path: &Path, content: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, content)?;
    }
    Ok(())
}

fn ensure_or_enrich_file(path: &Path, content: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, content)?;
        return Ok(());
    }

    let current = fs::read_to_string(path).unwrap_or_default();
    if current.trim().len() < 40 {
        fs::write(path, content)?;
    }
    Ok(())
}
