use crate::session::ChatMessage;
use anyhow::Result;

pub mod openai_compat;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: f32,
}

pub trait LlmClient {
    fn complete<'a>(
        &'a self,
        req: LlmRequest,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a;
}
