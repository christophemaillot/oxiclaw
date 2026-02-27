use anyhow::Result;
use chrono::Utc;
use std::env;
use log::{info, warn};
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use serde_json::Value;

use crate::config::AppConfig;
use crate::cron_store::{CronJobInput, CronStore};
use crate::curator;
use crate::engine::Engine;
use crate::llm::openai_compat::OpenAiCompatClient;
use crate::llm::{LlmClient, LlmRequest};
use crate::memory;
use crate::persona;
use crate::session::{ChatMessage, Session};
use crate::storage::TranscriptStore;
use crate::tools::ToolRegistry;

pub enum RuntimeEvent {
    Reply(String),
    Info(String),
    Exit,
}

#[derive(Debug)]
enum SupervisorEvent {
    CuratorStarted,
    CuratorTickOk(String),
    CuratorTickErr(String),
    CuratorStopped,
    CronStarted,
    CronRunOk(String),
    CronRunErr(String),
    CronNotify(String),
}

pub struct AgentRuntime {
    pub cfg: AppConfig,
    engine: Engine<OpenAiCompatClient>,
    session: Session,
    transcripts: TranscriptStore,
    cron_store: CronStore,
    supervisor_tx: mpsc::Sender<SupervisorEvent>,
    supervisor_rx: mpsc::Receiver<SupervisorEvent>,
    curator_task: Option<JoinHandle<()>>,
    cron_task: Option<JoinHandle<()>>,
}

impl AgentRuntime {
    pub fn new(cfg: AppConfig, debug_enabled: bool) -> Result<Self> {
        let llm = OpenAiCompatClient::new(cfg.endpoint.clone(), cfg.api_key.clone());
        let registry = ToolRegistry::with_defaults(cfg.basedir.clone());
        let tools_catalog = registry.specs_json_pretty();
        let system_prompt = persona::build_system_prompt(&cfg.basedir, &tools_catalog)?;

        info!("runtime:max_turn_steps={}", cfg.max_turn_steps);
        let warmup_enabled = env::var("OXICLAW_EMBED_WARMUP")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false);
        if warmup_enabled {
            match memory::warmup_embeddings(&cfg.basedir) {
                Ok(_) => info!("runtime:embedding_warmup=ok"),
                Err(e) => warn!("runtime:embedding_warmup=failed err={}", e),
            }
        } else {
            info!("runtime:embedding_warmup=disabled");
        }

        let engine = Engine::new(llm, cfg.model.clone(), registry)
            .with_debug(debug_enabled)
            .with_max_steps(cfg.max_turn_steps);
        let session = Session::new(system_prompt);
        let transcripts = TranscriptStore::new(cfg.transcripts_file.clone(), cfg.memory_index_file.clone())?;
        let cron_store = CronStore::init(&cfg.cron_db_file)?;
        info!("runtime:cron_db={}", cron_store.db_path().display());
        let (supervisor_tx, supervisor_rx) = mpsc::channel(256);

        let cron_task = Some(Self::spawn_cron_worker(
            cron_store.clone(),
            cfg.basedir.clone(),
            cfg.endpoint.clone(),
            cfg.model.clone(),
            cfg.api_key.clone(),
            cfg.telegram_bot_token.clone(),
            cfg.telegram_default_chat_id.clone(),
            supervisor_tx.clone(),
        ));

        Ok(Self {
            cfg,
            engine,
            session,
            transcripts,
            cron_store,
            supervisor_tx,
            supervisor_rx,
            curator_task: None,
            cron_task,
        })
    }

    pub fn help_text() -> String {
        [
            "/help            -> afficher l'aide",
            "/reset           -> vider l'historique (garde le system prompt)",
            "/reload-persona  -> relire SOUL.md / IDENTITY.md / USER.md / AGENT.md",
            "/curate          -> lancer le curator mémoire (conf/curator.toml)",
            "/curator-start   -> démarrer le curator en tâche parallèle",
            "/curator-stop    -> arrêter le curator parallèle",
            "/workers         -> état des workers",
            "/cron status     -> statut cron sqlite",
            "/cron add-curator-nightly -> ajoute un job systemEvent 'curate' nocturne",
            "/cron add-notify <texte> -> ajoute un job notify one-shot (at=now)",
            "/cron list       -> liste des jobs cron",
            "/cron run <job_id> -> queue une exécution manuelle",
            "/cron runs <job_id> -> historique des runs",
            "/quit            -> quitter",
        ]
        .join("\n")
    }

    pub async fn handle_line(&mut self, msg: &str) -> RuntimeEvent {
        self.drain_supervisor_events();

        match msg {
            "/quit" | "quit" | "exit" => RuntimeEvent::Exit,
            "/help" => RuntimeEvent::Info(Self::help_text()),
            "/reload-persona" => match self.reload_persona() {
                Ok(_) => RuntimeEvent::Info("Persona rechargée.".to_string()),
                Err(e) => RuntimeEvent::Info(format!("[erreur persona] {e}")),
            },
            "/reset" => {
                self.session.reset();
                let _ = self.transcripts.append(self.session.session_id(), "system", "[reset] Historique vidé.");
                RuntimeEvent::Info("Historique vidé.".to_string())
            }
            "/curate" => match curator::run_manual(
                &self.cfg.basedir,
                &self.cfg.endpoint,
                &self.cfg.model,
                &self.cfg.api_key,
            )
            .await
            {
                Ok(msg) => RuntimeEvent::Info(msg),
                Err(e) => RuntimeEvent::Info(format!("[erreur curator] {e}")),
            },
            "/curator-start" => RuntimeEvent::Info(self.start_curator_worker()),
            "/curator-stop" => RuntimeEvent::Info(self.stop_curator_worker()),
            "/workers" => RuntimeEvent::Info(self.workers_status()),
            _ if msg.starts_with("/cron") => RuntimeEvent::Info(self.handle_cron_command(msg)),
            _ => self.run_user_turn(msg).await,
        }
    }

    fn reload_persona(&mut self) -> Result<()> {
        let tools_catalog = ToolRegistry::with_defaults(self.cfg.basedir.clone()).specs_json_pretty();
        let new_prompt = persona::build_system_prompt(&self.cfg.basedir, &tools_catalog)?;
        self.session.set_system_prompt(new_prompt);
        let _ = self
            .transcripts
            .append(self.session.session_id(), "system", "[reload-persona] prompt rechargé depuis SOUL/IDENTITY/USER/AGENT");
        Ok(())
    }

    async fn run_user_turn(&mut self, msg: &str) -> RuntimeEvent {
        self.session.push_user(msg.to_string());
        let _ = self.transcripts.append(self.session.session_id(), "user", msg);

        match self.engine.run_turn(&mut self.session).await {
            Ok(answer) => {
                let _ = self.transcripts.append(self.session.session_id(), "assistant", &answer);
                RuntimeEvent::Reply(answer)
            }
            Err(e) => {
                let _ = self.transcripts.append(self.session.session_id(), "error", &e.to_string());
                self.session.rollback_last_user_if_any();
                RuntimeEvent::Info(format!("[erreur LLM] {e}"))
            }
        }
    }

    fn spawn_cron_worker(
        cron_store: CronStore,
        basedir: std::path::PathBuf,
        endpoint: String,
        model: String,
        api_key: String,
        telegram_bot_token: Option<String>,
        telegram_default_chat_id: Option<String>,
        tx: mpsc::Sender<SupervisorEvent>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _ = tx.send(SupervisorEvent::CronStarted).await;
            loop {
                if let Err(e) = cron_store.enqueue_due_jobs(Utc::now()) {
                    let _ = tx
                        .send(SupervisorEvent::CronRunErr(format!("enqueue error: {e}")))
                        .await;
                }

                match cron_store.claim_next_queued_run() {
                    Ok(Some(run)) => {
                        let result = execute_system_run(&run.payload_kind, &run.payload_json, &basedir, &endpoint, &model, &api_key).await;
                        match result {
                            Ok(output) => {
                                let _ = cron_store.finish_run_success(&run.run_id, &output.output_json);
                                if let Some(msg) = output.notify_main {
                                    if let (Some(token), Some(chat_id_raw)) = (
                                        telegram_bot_token.as_deref(),
                                        telegram_default_chat_id.as_deref(),
                                    ) {
                                        if let Ok(chat_id) = chat_id_raw.trim().parse::<i64>() {
                                            if let Err(e) = send_telegram_notify(token, chat_id, &msg).await {
                                                let _ = tx
                                                    .send(SupervisorEvent::CronRunErr(format!("notify telegram error: {e}")))
                                                    .await;
                                            }
                                        }
                                    }
                                    let _ = tx.send(SupervisorEvent::CronNotify(msg)).await;
                                }
                                let _ = tx
                                    .send(SupervisorEvent::CronRunOk(format!("job={} run={} ok", run.job_id, run.run_id)))
                                    .await;
                            }
                            Err(e) => {
                                let _ = cron_store.finish_run_failed(&run.run_id, &e.to_string());
                                let _ = tx
                                    .send(SupervisorEvent::CronRunErr(format!("job={} run={} err={}", run.job_id, run.run_id, e)))
                                    .await;
                            }
                        }
                    }
                    Ok(None) => {
                        sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(SupervisorEvent::CronRunErr(format!("store error: {e}")))
                            .await;
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        })
    }

    fn start_curator_worker(&mut self) -> String {
        if self.curator_task.is_some() {
            return "curator-worker: déjà démarré".to_string();
        }

        let basedir = self.cfg.basedir.clone();
        let endpoint = self.cfg.endpoint.clone();
        let model = self.cfg.model.clone();
        let api_key = self.cfg.api_key.clone();
        let tx = self.supervisor_tx.clone();

        let interval_secs = env::var("OXICLAW_CURATOR_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v >= 10)
            .unwrap_or(300);

        let handle = tokio::spawn(async move {
            let _ = tx.send(SupervisorEvent::CuratorStarted).await;
            loop {
                match curator::run_manual(&basedir, &endpoint, &model, &api_key).await {
                    Ok(msg) => {
                        let _ = tx.send(SupervisorEvent::CuratorTickOk(msg)).await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(SupervisorEvent::CuratorTickErr(e.to_string()))
                            .await;
                    }
                }
                sleep(Duration::from_secs(interval_secs)).await;
            }
        });

        self.curator_task = Some(handle);
        format!(
            "curator-worker: démarré (tick={}s, OXICLAW_CURATOR_INTERVAL_SECS)",
            interval_secs
        )
    }

    fn stop_curator_worker(&mut self) -> String {
        if let Some(handle) = self.curator_task.take() {
            handle.abort();
            let _ = self.supervisor_tx.try_send(SupervisorEvent::CuratorStopped);
            "curator-worker: arrêt demandé".to_string()
        } else {
            "curator-worker: déjà arrêté".to_string()
        }
    }

    fn workers_status(&self) -> String {
        let curator = if self.curator_task.is_some() {
            "running"
        } else {
            "stopped"
        };
        let cron = if self.cron_task.is_some() { "running" } else { "stopped" };
        format!(
            "workers:\n- curator: {curator}\n- cron: {cron}\ncron:\n- db: {}",
            self.cron_store.db_path().display()
        )
    }

    fn handle_cron_command(&self, msg: &str) -> String {
        let mut parts = msg.split_whitespace();
        let _ = parts.next(); // /cron
        match parts.next() {
            Some("status") => format!("cron: ok\n- db: {}", self.cron_store.db_path().display()),
            Some("list") => match self.cron_store.list_jobs(20) {
                Ok(rows) if rows.is_empty() => "cron: aucun job".to_string(),
                Ok(rows) => {
                    let mut out = vec![format!("cron: {} job(s)", rows.len())];
                    for j in rows {
                        out.push(format!(
                            "- {} | {} | schedule={} | payload={} | target={} | enabled={} | next={} | last={}",
                            j.id,
                            j.name.unwrap_or_else(|| "(sans nom)".to_string()),
                            j.schedule_kind,
                            j.payload_kind,
                            j.session_target,
                            j.enabled,
                            j.next_run_at.unwrap_or_else(|| "-".to_string()),
                            j.last_run_at.unwrap_or_else(|| "-".to_string())
                        ));
                    }
                    out.join("\n")
                }
                Err(e) => format!("cron list: erreur: {e}"),
            },
            Some("add-curator-nightly") => {
                let payload = json!({
                    "type":"curate",
                    "version":1,
                    "data":{"mode":"incremental","window":"24h"}
                })
                .to_string();
                let schedule = json!({"kind":"cron","expr":"0 2 * * *","tz":"Europe/Paris"}).to_string();

                let input = CronJobInput {
                    name: Some("curator-nightly".to_string()),
                    schedule_kind: "cron".to_string(),
                    schedule_json: schedule,
                    payload_kind: "systemEvent".to_string(),
                    payload_json: payload,
                    session_target: "main".to_string(),
                    next_run_at: None,
                };

                match self.cron_store.add_job(input) {
                    Ok(job_id) => format!("cron add: ok\n- job_id: {job_id}\n- name: curator-nightly"),
                    Err(e) => format!("cron add: erreur: {e}"),
                }
            }
            Some("add-notify") => {
                let text = parts.collect::<Vec<_>>().join(" ");
                if text.trim().is_empty() {
                    return "usage: /cron add-notify <texte>".to_string();
                }

                let payload = json!({
                    "type":"notify",
                    "version":1,
                    "data":{"message": text}
                })
                .to_string();
                let now = chrono::Utc::now().to_rfc3339();
                let schedule = json!({"kind":"at","at": now}).to_string();

                let input = CronJobInput {
                    name: Some("notify-once".to_string()),
                    schedule_kind: "at".to_string(),
                    schedule_json: schedule,
                    payload_kind: "systemEvent".to_string(),
                    payload_json: payload,
                    session_target: "main".to_string(),
                    next_run_at: Some(now),
                };

                match self.cron_store.add_job(input) {
                    Ok(job_id) => match self.cron_store.trigger_run_manual(&job_id) {
                        Ok(run_id) => format!("cron add-notify: ok\n- job_id: {job_id}\n- run_id: {run_id}"),
                        Err(e) => format!("cron add-notify: job créé mais run ko: {e}"),
                    },
                    Err(e) => format!("cron add-notify: erreur: {e}"),
                }
            }
            Some("run") => {
                let Some(job_id) = parts.next() else {
                    return "usage: /cron run <job_id>".to_string();
                };
                match self.cron_store.trigger_run_manual(job_id) {
                    Ok(run_id) => format!("cron run: queued\n- job_id: {job_id}\n- run_id: {run_id}"),
                    Err(e) => format!("cron run: erreur: {e}"),
                }
            }
            Some("runs") => {
                let Some(job_id) = parts.next() else {
                    return "usage: /cron runs <job_id>".to_string();
                };
                match self.cron_store.list_runs(job_id, 20) {
                    Ok(rows) if rows.is_empty() => format!("cron runs: aucun run pour {job_id}"),
                    Ok(rows) => {
                        let mut out = vec![format!("cron runs: {}", job_id)];
                        for r in rows {
                            out.push(format!(
                                "- {} | job={} | status={} | trigger={} | at={}",
                                r.run_id, r.job_id, r.status, r.trigger_source, r.created_at
                            ));
                        }
                        out.join("\n")
                    }
                    Err(e) => format!("cron runs: erreur: {e}"),
                }
            }
            _ => "usage: /cron status|list|add-curator-nightly|add-notify <texte>|run <job_id>|runs <job_id>".to_string(),
        }
    }

    fn drain_supervisor_events(&mut self) {
        while let Ok(ev) = self.supervisor_rx.try_recv() {
            let line = match ev {
                SupervisorEvent::CuratorStarted => "[worker] curator started".to_string(),
                SupervisorEvent::CuratorTickOk(msg) => format!("[worker] curator ok: {msg}"),
                SupervisorEvent::CuratorTickErr(err) => format!("[worker] curator error: {err}"),
                SupervisorEvent::CuratorStopped => "[worker] curator stopped".to_string(),
                SupervisorEvent::CronStarted => "[worker] cron started".to_string(),
                SupervisorEvent::CronRunOk(msg) => format!("[worker] cron ok: {msg}"),
                SupervisorEvent::CronRunErr(err) => format!("[worker] cron error: {err}"),
                SupervisorEvent::CronNotify(msg) => {
                    let notify_line = format!("[cron notify] {msg}");
                    self.session.push_system(notify_line.clone());
                    notify_line
                }
            };
            let _ = self.transcripts.append(self.session.session_id(), "system", &line);
        }
    }
}

async fn send_telegram_notify(bot_token: &str, chat_id: i64, text: &str) -> Result<()> {
    let client = Client::new();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
    client
        .post(url)
        .json(&json!({"chat_id": chat_id, "text": text}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

struct SystemRunOutput {
    output_json: String,
    notify_main: Option<String>,
}

async fn execute_system_run(
    payload_kind: &str,
    payload_json: &str,
    basedir: &std::path::Path,
    endpoint: &str,
    model: &str,
    api_key: &str,
) -> Result<SystemRunOutput> {
    match payload_kind {
        "systemEvent" => {
            let payload: Value = serde_json::from_str(payload_json)?;
            let event_type = payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            match event_type {
                "curate" => {
                    let msg = curator::run_manual(
                        &basedir.to_path_buf(),
                        endpoint,
                        model,
                        api_key,
                    )
                    .await?;
                    Ok(SystemRunOutput {
                        output_json: json!({"event":"curate","ok":true,"message":msg}).to_string(),
                        notify_main: None,
                    })
                }
                "notify" => {
                    let message = payload
                        .get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("notification");
                    Ok(SystemRunOutput {
                        output_json: json!({"event":"notify","ok":true,"message":message}).to_string(),
                        notify_main: Some(message.to_string()),
                    })
                }
                other => anyhow::bail!("systemEvent non supporté: {other}"),
            }
        }
        "agentTurn" => {
            let payload: Value = serde_json::from_str(payload_json)?;
            let message = payload
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("agentTurn: message manquant"))?;
            let run_model = payload
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or(model)
                .to_string();

            let client = OpenAiCompatClient::new(endpoint.to_string(), api_key.to_string());
            let messages = vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: "Tu es un sous-agent cron. Réponds de manière concise et actionnable.".to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: message.to_string(),
                },
            ];

            let answer = client
                .complete(LlmRequest {
                    model: run_model,
                    messages,
                    temperature: 0.2,
                })
                .await?;

            let notify = format!("[cron agentTurn] {}", answer.chars().take(300).collect::<String>());
            Ok(SystemRunOutput {
                output_json: json!({"kind":"agentTurn","ok":true,"answer":answer}).to_string(),
                notify_main: Some(notify),
            })
        }
        other => anyhow::bail!("payload_kind non supporté: {other}"),
    }
}
