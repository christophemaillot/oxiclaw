use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::llm::{LlmClient, LlmRequest};
use crate::session::ChatMessage;

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    endpoint: String,
    api_key: String,
}

impl OpenAiCompatClient {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            http: Client::new(),
            endpoint,
            api_key,
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

impl LlmClient for OpenAiCompatClient {
    async fn complete(&self, req: LlmRequest) -> Result<String> {
        let body = ChatRequest {
            model: req.model,
            messages: req.messages,
            temperature: req.temperature,
        };

        let res = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("requête HTTP")?;

        let status = res.status();
        let text = res.text().await.context("lecture réponse")?;

        if !status.is_success() {
            anyhow::bail!("HTTP {}: {}", status, text);
        }

        let parsed: ChatResponse = serde_json::from_str(&text).context("parse JSON chat/completions")?;

        let answer = parsed
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_else(|| "(réponse vide)".to_string());

        Ok(answer)
    }
}
