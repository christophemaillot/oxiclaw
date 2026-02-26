use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::runtime::{AgentRuntime, RuntimeEvent};

#[derive(Clone)]
struct AppState {
    runtime: Arc<Mutex<AgentRuntime>>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    reply: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
}

pub async fn serve(runtime: AgentRuntime, host: String, port: u16) -> Result<()> {
    let state = AppState {
        runtime: Arc::new(Mutex::new(runtime)),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/chat", post(chat))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    println!("HTTP server listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Json<ChatResponse> {
    let mut rt = state.runtime.lock().await;
    let response = match rt.handle_line(req.message.trim()).await {
        RuntimeEvent::Reply(text) | RuntimeEvent::Info(text) => text,
        RuntimeEvent::Exit => "Session closed".to_string(),
    };

    Json(ChatResponse { reply: response })
}
