use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{oneshot, Mutex};

use crate::runtime::{AgentRuntime, RuntimeEvent};

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

pub async fn serve(runtime: AgentRuntime) -> Result<()> {
    let token = runtime
        .cfg
        .telegram_bot_token
        .clone()
        .context("Telegram activé mais token manquant (TELEGRAM_BOT_TOKEN ou conf/config.toml.telegram.bot_token)")?;

    let allowed_chat_id = runtime
        .cfg
        .telegram_default_chat_id
        .as_ref()
        .and_then(|v| v.trim().parse::<i64>().ok());

    let runtime = Arc::new(Mutex::new(runtime));
    let client = Client::new();
    let base = format!("https://api.telegram.org/bot{token}");

    println!("Telegram polling démarré.");
    if let Some(chat_id) = allowed_chat_id {
        println!("Filtrage chat actif: {chat_id}");
    }

    let mut offset: i64 = 0;

    loop {
        let updates = match get_updates(&client, &base, offset).await {
            Ok(v) => v,
            Err(err) => {
                eprintln!("[telegram] getUpdates error: {err}");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        if !updates.ok {
            eprintln!("[telegram] getUpdates returned ok=false");
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }

        for update in updates.result {
            offset = update.update_id + 1;

            let Some(message) = update.message else {
                continue;
            };

            let Some(text) = message.text.map(|t| t.trim().to_string()) else {
                continue;
            };

            if text.is_empty() {
                continue;
            }

            let chat_id = message.chat.id;

            if let Some(allowed) = allowed_chat_id {
                if chat_id != allowed {
                    eprintln!("[telegram] message ignoré (chat non autorisé: {chat_id})");
                    continue;
                }
            }

            let normalized_text = normalize_telegram_command(&text);

            let (presence_stop_tx, mut presence_stop_rx) = oneshot::channel::<()>();
            let presence_client = client.clone();
            let presence_base = base.clone();

            let presence_task = tokio::spawn(async move {
                if let Err(err) = send_chat_action(&presence_client, &presence_base, chat_id, "typing").await {
                    eprintln!("[telegram presence] sendChatAction failed: {err}");
                }

                loop {
                    tokio::select! {
                        _ = &mut presence_stop_rx => {
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(4)) => {
                            if let Err(err) = send_chat_action(&presence_client, &presence_base, chat_id, "typing").await {
                                eprintln!("[telegram presence] sendChatAction failed: {err}");
                            }
                        }
                    }
                }
            });

            let reply = {
                let mut rt = runtime.lock().await;

                if normalized_text == "/start" {
                    "Salut 👋 Je suis là si tu veux. Dis-moi juste ce qu’on fait.".to_string()
                } else {
                    match rt.handle_line(&normalized_text).await {
                        RuntimeEvent::Reply(text) | RuntimeEvent::Info(text) => text,
                        RuntimeEvent::Exit => "Session fermée (/quit ignoré en mode Telegram).".to_string(),
                    }
                }
            };

            let _ = presence_stop_tx.send(());
            let _ = presence_task.await;

            if let Err(err) = send_message(&client, &base, chat_id, &reply).await {
                eprintln!("[telegram] sendMessage failed: {err}");
            }
        }
    }
}

async fn get_updates(client: &Client, base: &str, offset: i64) -> Result<GetUpdatesResponse> {
    let url = format!("{base}/getUpdates");
    let response = client
        .post(url)
        .json(&json!({
            "timeout": 30,
            "offset": offset,
            "allowed_updates": ["message"]
        }))
        .send()
        .await?
        .error_for_status()?;

    Ok(response.json::<GetUpdatesResponse>().await?)
}

async fn send_message(client: &Client, base: &str, chat_id: i64, text: &str) -> Result<()> {
    let url = format!("{base}/sendMessage");
    client
        .post(url)
        .json(&json!({
            "chat_id": chat_id,
            "text": text
        }))
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

async fn send_chat_action(client: &Client, base: &str, chat_id: i64, action: &str) -> Result<()> {
    let url = format!("{base}/sendChatAction");
    client
        .post(url)
        .json(&json!({
            "chat_id": chat_id,
            "action": action
        }))
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

fn normalize_telegram_command(input: &str) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return trimmed.to_string();
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or(trimmed);
    let rest = parts.next().unwrap_or("").trim();

    let cmd_clean = cmd
        .split('@')
        .next()
        .unwrap_or(cmd)
        .trim()
        .to_string();

    if rest.is_empty() {
        cmd_clean
    } else {
        format!("{cmd_clean} {rest}")
    }
}
