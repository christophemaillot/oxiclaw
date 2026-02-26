use anyhow::Result;
use std::env;
use std::path::PathBuf;

#[path = "../memory.rs"]
mod memory;

fn main() -> Result<()> {
    let mut basedir = PathBuf::from("./basedir");
    let mut query = String::new();
    let mut limit: usize = 5;
    let mut archive = false;
    let mut reindex = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--basedir" => {
                if let Some(v) = args.next() {
                    basedir = PathBuf::from(v);
                }
            }
            "--query" => {
                if let Some(v) = args.next() {
                    query = v;
                }
            }
            "--limit" => {
                if let Some(v) = args.next() {
                    limit = v.parse::<usize>().unwrap_or(5).max(1);
                }
            }
            "--archive" => archive = true,
            "--reindex" => reindex = true,
            _ => {}
        }
    }

    if reindex {
        memory::run_indexer_once(&basedir)?;
    }

    if query.trim().is_empty() {
        eprintln!("Usage: cargo run --bin memory_probe -- --basedir <path> --query <text> [--limit 5] [--archive] [--reindex]");
        std::process::exit(2);
    }

    let rows = memory::memory_search(
        &basedir,
        &query,
        memory::MemorySearchOptions { limit, archive },
    )?;

    if rows.is_empty() {
        println!("(no results)");
        return Ok(());
    }

    for (i, row) in rows.iter().enumerate() {
        println!("{}. {}", i + 1, row);
    }

    Ok(())
}
