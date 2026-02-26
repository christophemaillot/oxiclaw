use anyhow::Result;
use std::env;
use log::{info, warn};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use crate::config::AppConfig;
use crate::cron_store::{CronJobInput, CronStore};
use crate::curator;
use crate::engine::Engine;
use crate::llm::openai_compat::OpenAiCompatClient;
use crate::memory;
use crate::persona;
use crate::session::Session;
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

        Ok(Self {
            cfg,
            engine,
            session,
            transcripts,
            cron_store,
            supervisor_tx,
            supervisor_rx,
            curator_task: None,
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
        format!(
            "workers:\n- curator: {curator}\ncron:\n- db: {}",
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
            _ => "usage: /cron status|list|add-curator-nightly|run <job_id>|runs <job_id>".to_string(),
        }
    }

    fn drain_supervisor_events(&mut self) {
        while let Ok(ev) = self.supervisor_rx.try_recv() {
            let line = match ev {
                SupervisorEvent::CuratorStarted => "[worker] curator started".to_string(),
                SupervisorEvent::CuratorTickOk(msg) => format!("[worker] curator ok: {msg}"),
                SupervisorEvent::CuratorTickErr(err) => format!("[worker] curator error: {err}"),
                SupervisorEvent::CuratorStopped => "[worker] curator stopped".to_string(),
            };
            let _ = self.transcripts.append(self.session.session_id(), "system", &line);
        }
    }
}
