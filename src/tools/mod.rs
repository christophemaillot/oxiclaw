use chrono::Utc;
use ureq;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::memory;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub args_schema: Value,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn run(&self, args: &Value) -> String;
}

pub struct TimeTool;
impl Tool for TimeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "time",
            description: "Retourne l'heure courante en UTC (RFC3339)",
            args_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, _args: &Value) -> String {
        Utc::now().to_rfc3339()
    }
}

pub struct EchoTool;
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo",
            description: "Répète exactement le texte fourni",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Texte à répéter"}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, args: &Value) -> String {
        args.get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(echo sans texte)".to_string())
    }
}

pub struct HttpRequestTool;

impl Tool for HttpRequestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_request",
            description: "Exécute une requête HTTP (GET/POST/PUT/PATCH/DELETE...) avec timeout, headers et payload JSON/texte.",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "description": "Méthode HTTP (GET par défaut)"},
                    "url": {"type": "string", "description": "URL absolue http(s)"},
                    "headers": {"type": "object", "description": "Headers HTTP (clé -> valeur string)"},
                    "query": {"type": "object", "description": "Paramètres query string (clé -> string/number/bool)"},
                    "json": {"description": "Corps JSON à envoyer"},
                    "body": {"type": "string", "description": "Corps texte brut (si json absent)"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000, "description": "Timeout réseau"},
                    "max_chars": {"type": "integer", "minimum": 200, "maximum": 20000, "description": "Troncature du body de réponse"}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, args: &Value) -> String {
        let method = args
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();

        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(v) if v.starts_with("http://") || v.starts_with("https://") => v,
            Some(_) => return "erreur http_request: url doit commencer par http:// ou https://".to_string(),
            None => return "erreur http_request: url manquante".to_string(),
        };

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(15_000)
            .clamp(100, 120_000);

        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(4_000)
            .clamp(200, 20_000) as usize;

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(timeout_ms))
            .build();

        let mut req = agent.request(&method, url);

        if let Some(headers_obj) = args.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers_obj {
                let Some(val_str) = v.as_str() else {
                    return format!("erreur http_request: header '{k}' doit être une string");
                };
                req = req.set(k, val_str);
            }
        }

        if let Some(query_obj) = args.get("query").and_then(|v| v.as_object()) {
            for (k, v) in query_obj {
                let value = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Null => "null".to_string(),
                    _ => return format!("erreur http_request: query '{k}' doit être string/number/bool/null"),
                };
                req = req.query(k, &value);
            }
        }

        let response = if let Some(json_body) = args.get("json") {
            req.send_json(json_body.clone())
        } else if let Some(raw_body) = args.get("body").and_then(|v| v.as_str()) {
            req.send_string(raw_body)
        } else {
            req.call()
        };

        let (ok, status, final_url, content_type, body_text) = match response {
            Ok(resp) => {
                let status = resp.status();
                let final_url = resp.get_url().to_string();
                let content_type = resp.header("content-type").unwrap_or("").to_string();
                let body_text = resp.into_string().unwrap_or_else(|e| format!("[body read error: {e}]"));
                (status >= 200 && status < 300, status, final_url, content_type, body_text)
            }
            Err(ureq::Error::Status(status, resp)) => {
                let final_url = resp.get_url().to_string();
                let content_type = resp.header("content-type").unwrap_or("").to_string();
                let body_text = resp.into_string().unwrap_or_else(|e| format!("[body read error: {e}]"));
                (false, status, final_url, content_type, body_text)
            }
            Err(ureq::Error::Transport(e)) => {
                return format!("erreur http_request: transport: {e}");
            }
        };

        let truncated = if body_text.chars().count() > max_chars {
            let cut: String = body_text.chars().take(max_chars).collect();
            format!("{cut}\n...[truncated]")
        } else {
            body_text
        };

        serde_json::to_string_pretty(&json!({
            "ok": ok,
            "status": status,
            "method": method,
            "url": final_url,
            "content_type": content_type,
            "body": truncated
        }))
        .unwrap_or_else(|e| format!("erreur http_request: sérialisation résultat: {e}"))
    }
}

pub struct InfoAppendTool {
    basedir: PathBuf,
}

impl InfoAppendTool {
    pub fn new(basedir: PathBuf) -> Self {
        Self { basedir }
    }
}

impl Tool for InfoAppendTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "info_append",
            description: "Ajoute une note append-only dans AGENT.md ou USER.md. SOUL.md est interdit.",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["agent", "user"]},
                    "text": {"type": "string", "description": "Information stable et utile à retenir"}
                },
                "required": ["target", "text"],
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, args: &Value) -> String {
        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();

        if text.is_empty() {
            return "erreur info_append: text vide".to_string();
        }
        if text.len() > 500 {
            return "erreur info_append: text trop long (>500)".to_string();
        }

        let lower = text.to_lowercase();
        let blocked = ["api_key", "token", "password", "secret", "bearer ", "ssh-"];
        if blocked.iter().any(|w| lower.contains(w)) {
            return "erreur info_append: contenu sensible détecté, refusé".to_string();
        }

        let path = match target {
            "agent" => self.basedir.join("AGENT.md"),
            "user" => self.basedir.join("USER.md"),
            "soul" => return "erreur info_append: SOUL.md est immuable".to_string(),
            _ => return "erreur info_append: target doit être 'agent' ou 'user'".to_string(),
        };

        let existing = fs::read_to_string(&path).unwrap_or_default();
        if existing.lines().any(|line| line.contains(text)) {
            return format!("info_append: déjà présent dans {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("fichier"));
        }

        let ts = Utc::now().to_rfc3339();
        let line = format!("\n- [{}] {}", ts, text);

        let mut f = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => return format!("erreur info_append: open: {e}"),
        };

        if let Err(e) = f.write_all(line.as_bytes()) {
            return format!("erreur info_append: write: {e}");
        }

        format!(
            "info_append: ajouté dans {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("fichier")
        )
    }
}

pub struct MemorySearchTool {
    basedir: PathBuf,
}

impl MemorySearchTool {
    pub fn new(basedir: PathBuf) -> Self {
        Self { basedir }
    }
}

pub struct MemoryGetTool {
    basedir: PathBuf,
}

impl MemoryGetTool {
    pub fn new(basedir: PathBuf) -> Self {
        Self { basedir }
    }
}

impl Tool for MemorySearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_search",
            description: "Cherche des souvenirs pertinents dans les index mémoire construits depuis les transcripts",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Question ou mots-clés"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10},
                    "archive": {"type": "boolean", "description": "Inclut les transcripts anciens (archive), false par défaut"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, args: &Value) -> String {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;
        let archive = args
            .get("archive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if query.trim().is_empty() {
            return "query vide".to_string();
        }

        match memory::memory_search(
            &self.basedir,
            query,
            memory::MemorySearchOptions { limit, archive },
        ) {
            Ok(rows) if rows.is_empty() => "Aucun souvenir trouvé.".to_string(),
            Ok(rows) => rows.join("\n"),
            Err(e) => format!("erreur memory_search: {e}"),
        }
    }
}

impl Tool for MemoryGetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_get",
            description: "Lit un souvenir précis à partir de son identifiant renvoyé par memory_search",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Identifiant d'entrée renvoyé par memory_search (ex: mem-42-7ab3...)"}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn run(&self, args: &Value) -> String {
        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.trim().is_empty() {
            return "id manquant".to_string();
        }

        match memory::memory_get(&self.basedir, id) {
            Ok(Some(row)) => row,
            Ok(None) => format!("Aucune entrée pour id={id}"),
            Err(e) => format!("erreur memory_get: {e}"),
        }
    }
}

pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults(basedir: PathBuf) -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };
        registry.register(Box::new(TimeTool));
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(HttpRequestTool));
        registry.register(Box::new(InfoAppendTool::new(basedir.clone())));
        registry.register(Box::new(MemorySearchTool::new(basedir.clone())));
        registry.register(Box::new(MemoryGetTool::new(basedir)));
        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
    }

    pub fn execute(&self, name: &str, args: &Value) -> String {
        match self.tools.get(name) {
            Some(tool) => tool.run(args),
            None => format!("tool inconnu: {name}"),
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn specs_json_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.specs()).unwrap_or_else(|_| "[]".to_string())
    }
}

#[derive(Debug, Clone)]
pub enum ModelAction {
    ToolCall { name: String, args: Value },
    FinalAnswer(String),
}

pub fn parse_model_action(raw: &str) -> Result<ModelAction, String> {
    let trimmed = raw.trim();

    // Compat pragmatique: texte brut => réponse finale.
    if !trimmed.starts_with('{') {
        return Ok(ModelAction::FinalAnswer(trimmed.to_string()));
    }

    let v: Value = serde_json::from_str(trimmed).map_err(|e| format!("JSON invalide: {e}"))?;

    let t = v
        .get("type")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "champ 'type' manquant".to_string())?;

    match t {
        "tool_call" => {
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "champ 'name' manquant pour tool_call".to_string())?
                .to_string();
            let args = v
                .get("args")
                .cloned()
                .unwrap_or(Value::Object(Default::default()));
            Ok(ModelAction::ToolCall { name, args })
        }
        "final_answer" => {
            let answer = v
                .get("answer")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "champ 'answer' manquant pour final_answer".to_string())?
                .to_string();
            Ok(ModelAction::FinalAnswer(answer))
        }
        other => Err(format!(
            "type inconnu '{other}' (attendu: tool_call ou final_answer)"
        )),
    }
}

pub fn escape_for_json_string(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
