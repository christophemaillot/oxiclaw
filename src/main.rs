use anyhow::{Context, Result};
use std::env;
use std::io::{self, Write};

use log::info;

mod config;
mod engine;
mod llm;
mod persona;
mod runtime;
mod session;
mod storage;
mod memory;
mod tools;
mod http;
mod telegram;
mod curator;
mod cron_store;

use config::AppConfig;
use runtime::{AgentRuntime, RuntimeEvent};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cfg = AppConfig::from_env_and_args()?;

    let debug_enabled = env::var("OXICLAW_DEBUG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false);

    init_logger(debug_enabled);
    info!("oxiclaw:start basedir={} model={} endpoint={}", cfg.basedir.display(), cfg.model, cfg.endpoint);

    let mut rt = AgentRuntime::new(cfg, debug_enabled)?;

    println!("🦀 OxiClaw prêt");
    println!("Modèle: {}", rt.cfg.model);
    println!("Endpoint: {}", rt.cfg.endpoint);
    println!("BaseDir: {}", rt.cfg.basedir.display());
    println!("Config: {}", rt.cfg.conf_file.display());
    println!("Transcripts: {}", rt.cfg.transcripts_file.display());
    println!("Memory index: {}", rt.cfg.memory_index_file.display());
    println!("Cron DB: {}", rt.cfg.cron_db_file.display());
    println!(
        "Debug: {} (OXICLAW_DEBUG=1 pour activer)",
        if debug_enabled { "ON" } else { "OFF" }
    );

    if has_flag("--http") {
        let host = rt.cfg.http_host.clone();
        let port = rt.cfg.http_port;
        return http::serve(rt, host, port).await;
    }

    if has_flag("--telegram") || rt.cfg.telegram_enabled {
        return telegram::serve(rt).await;
    }

    println!("Commandes: /help, /reset, /reload-persona, /quit\n");

    loop {
        print!("> ");
        io::stdout().flush().context("flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("lecture stdin")?;
        let msg = input.trim();

        if msg.is_empty() {
            continue;
        }

        print!("oxiclaw> ");
        io::stdout().flush().ok();

        match rt.handle_line(msg).await {
            RuntimeEvent::Reply(answer) | RuntimeEvent::Info(answer) => println!("{answer}"),
            RuntimeEvent::Exit => {
                println!("Bye 👋");
                break;
            }
        }
    }

    Ok(())
}

fn has_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

fn init_logger(debug_enabled: bool) {
    let default_level = if debug_enabled {
        "debug,tantivy=info"
    } else {
        "info,tantivy=warn"
    };
    let env = env_logger::Env::default().filter_or("OXICLAW_LOG", default_level);
    let _ = env_logger::Builder::from_env(env)
        .format_timestamp_secs()
        .try_init();
}
