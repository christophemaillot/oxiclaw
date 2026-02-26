use anyhow::Result;
use log::info;

use crate::llm::{LlmClient, LlmRequest};
use crate::session::Session;
use crate::tools::{escape_for_json_string, parse_model_action, ModelAction, ToolRegistry};

pub struct Engine<C: LlmClient> {
    client: C,
    model: String,
    tools: ToolRegistry,
    max_steps: usize,
    debug: bool,
}

impl<C: LlmClient> Engine<C> {
    pub fn new(client: C, model: String, tools: ToolRegistry) -> Self {
        Self {
            client,
            model,
            tools,
            max_steps: 6,
            debug: false,
        }
    }

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    fn log(&self, line: &str) {
        if self.debug {
            eprintln!("[oxiclaw:debug] {line}");
        }
    }

    pub async fn run_turn(&self, session: &mut Session) -> Result<String> {
        self.log(&format!("turn:start max_steps={}", self.max_steps));

        for step in 0..self.max_steps {
            self.log(&format!("step={} request:send", step + 1));

            let req = LlmRequest {
                model: self.model.clone(),
                messages: session.messages(),
                temperature: 0.2,
            };

            let assistant_output = self.client.complete(req).await?;
            self.log(&format!(
                "step={} response: {}",
                step + 1,
                assistant_output.chars().take(180).collect::<String>().replace('\n', "\\n")
            ));

            match parse_model_action(&assistant_output) {
                Ok(ModelAction::ToolCall { name, args }) => {
                    self.log(&format!("step={} action=tool_call name={} args={}", step + 1, name, args));
                    let args_compact = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                    info!("tool_call: {}({})", name, args_compact);

                    let tool_result = self.tools.execute(&name, &args);
                    let preview: String = tool_result.chars().take(220).collect();
                    info!("tool_result: {} -> {}", name, preview.replace('\n', "\\n"));

                    self.log(&format!("step={} tool_result={}", step + 1, tool_result));
                    session.push_assistant(assistant_output);
                    session.push_system(format!(
                        "TOOL_RESULT {{\"name\":\"{}\",\"output\":\"{}\"}}",
                        name,
                        escape_for_json_string(&tool_result)
                    ));
                    continue;
                }
                Ok(ModelAction::FinalAnswer(answer)) => {
                    self.log(&format!("step={} action=final_answer", step + 1));
                    session.push_assistant(assistant_output);
                    self.log("turn:end success");
                    return Ok(answer);
                }
                Err(parse_err) => {
                    self.log(&format!("step={} action=parse_error err={}", step + 1, parse_err));
                    // Boucle de réparation de protocole: on corrige et on redemande.
                    session.push_assistant(assistant_output);
                    session.push_system(format!(
                        "PROTOCOLE_INVALIDE: {parse_err}. Réponds MAINTENANT en JSON strict avec exactement l'un des formats suivants:\n{{\"type\":\"tool_call\",\"name\":\"<tool_name>\",\"args\":{{...}}}}\nou\n{{\"type\":\"final_answer\",\"answer\":\"<texte>\"}}"
                    ));
                    continue;
                }
            }
        }

        if let Some((_, last_tool_result)) = session.last_tool_result() {
            self.log("turn:end fallback_last_tool_result");
            return Ok(format!("(fallback) {last_tool_result}"));
        }

        self.log("turn:end error=max_steps_reached");
        anyhow::bail!("Trop d'étapes dans le turn (boucle interrompue)")
    }
}
