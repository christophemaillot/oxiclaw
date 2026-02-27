# OxiClaw

> ⚠️ **Experimental project**
>
> OxiClaw is an active lab project. Architecture, APIs, and behavior may change quickly.

OxiClaw is a Rust agent runtime inspired by OpenClaw, focused on clarity, robustness, and fast iteration.

## What is implemented today

- **Multi-step agent loop** (LLM → tool calls → final answer) with protocol repair and step limits.
- **Persistent transcripts** in JSONL, with session tracking.
- **Searchable memory** through `memory_search` and `memory_get`.
- **Hybrid memory indexing**:
  - lexical index (Tantivy),
  - vector index,
  - **RRF (Reciprocal Rank Fusion)** for result fusion.
- **Session-aware memory retrieval** to reduce self-contamination from the current session.
- **HTTP mode** (`/health`, `/chat`) and **Telegram mode** (long polling).
- **Reloadable runtime persona** (`SOUL.md`, `IDENTITY.md`, `USER.md`, `AGENT.md`).

## Cron subsystem (current state)

OxiClaw now includes a first internal cron implementation based on SQLite.

### Storage

- `state/cron.sqlite`
- `cron_jobs`
- `cron_job_runs`
- `cron_scheduler_state`

### Scheduler / runner

- Background scheduler loop enqueues due jobs.
- Worker claims queued runs and executes them.
- Run lifecycle tracked in DB (`queued` → `running` → `succeeded` / `failed`).

### Payload kinds

- `systemEvent`
  - `notify` (pushes to main session context, and Telegram proactive send when configured)
  - `curate` (runs memory curator)
- `agentTurn` (minimal version)
  - executes an isolated LLM turn from a scheduled prompt
  - stores output in `cron_job_runs.output_json`
  - posts a short summary back to main session context

### Tooling (minimal for now)

- `cron_manage` tool with limited actions:
  - `add_notify`
  - `add_agent_turn`
  - `list`
  - `runs`
  - `run`

### Prompt/config iteration without rebuild

Tool descriptions are now externalized in:

- `conf/prompts/tools.toml`

This allows behavior tuning (especially tool usage guidance) without recompiling.

## Quick start

```bash
cargo run -- --basedir ./oxiclaw-home
```

HTTP mode:

```bash
cargo run -- --basedir ./oxiclaw-home --http
```

Telegram mode:

```bash
TELEGRAM_BOT_TOKEN=xxx cargo run -- --basedir ./oxiclaw-home --telegram
```

## Status

This repository is public to share experiments early, document architectural choices, and iterate in the open.