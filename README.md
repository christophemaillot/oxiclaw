# OxiClaw

> ⚠️ **Projet expérimental**
>
> OxiClaw est un labo en cours de construction. L’architecture, les APIs et les comportements peuvent changer rapidement.

OxiClaw est un moteur d’agent en Rust (inspiré d’OpenClaw), orienté simplicité, robustesse et itération rapide.

## Ce qui est déjà en place

- **Boucle agentique multi-étapes** (LLM → tool calls → réponse finale), avec garde-fous.
- **Transcripts persistés** en JSONL, avec suivi de session.
- **Mémoire interrogeable** via tools `memory_search` et `memory_get`.
- **Indexation mémoire hybride** :
  - index lexical (Tantivy),
  - index vectoriel,
  - fusion des résultats par **RRF (Reciprocal Rank Fusion)**.
- **Isolation de session mémoire** (évite l’auto-contamination de la session courante).
- **Mode HTTP** (`/health`, `/chat`) et **mode Telegram** (long polling).
- **Persona runtime** rechargeable (`SOUL.md`, `IDENTITY.md`, `USER.md`).

## Positionnement actuel

Le mode lexical est stable. Le vectoriel est activable de façon contrôlée selon la machine cible et les contraintes mémoire.

## Démarrage rapide

```bash
cargo run -- --basedir ./oxiclaw-home
```

Mode HTTP :

```bash
cargo run -- --basedir ./oxiclaw-home --http
```

Mode Telegram :

```bash
TELEGRAM_BOT_TOKEN=xxx cargo run -- --basedir ./oxiclaw-home --telegram
```

## Statut

Ce dépôt est public pour partager l’expérimentation, documenter les choix techniques, et recueillir des retours tôt.