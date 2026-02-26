use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    history: Vec<ChatMessage>,
    session_id: String,
}

impl Session {
    pub fn new(system_prompt: String) -> Self {
        Self {
            history: vec![ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
            }],
            session_id: Uuid::new_v4().to_string(),
        }
    }

    pub fn reset(&mut self) {
        self.history.truncate(1);
        self.session_id = Uuid::new_v4().to_string();
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn set_system_prompt(&mut self, system_prompt: String) {
        if self.history.is_empty() {
            self.history.push(ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
            });
            return;
        }

        self.history[0] = ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        };
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.history.push(ChatMessage {
            role: "user".to_string(),
            content: text.into(),
        });
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.history.push(ChatMessage {
            role: "assistant".to_string(),
            content: text.into(),
        });
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.history.push(ChatMessage {
            role: "system".to_string(),
            content: text.into(),
        });
    }

    pub fn rollback_last_user_if_any(&mut self) {
        if let Some(last) = self.history.last() {
            if last.role == "user" {
                self.history.pop();
            }
        }
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        self.history.clone()
    }

    pub fn last_tool_result(&self) -> Option<(String, String)> {
        for msg in self.history.iter().rev() {
            if msg.content.starts_with("TOOL_RESULT ") {
                let json_part = msg.content.strip_prefix("TOOL_RESULT ")?.lines().next()?;
                let v: serde_json::Value = serde_json::from_str(json_part).ok()?;
                let name = v.get("name")?.as_str()?.to_string();
                let output = v.get("output")?.as_str()?.to_string();
                return Some((name, output));
            }
        }
        None
    }
}
