# OxiClaw Indexer — Sous-projet

Objectif: fournir un module d’indexation **indépendant** du runtime agent, pour rechercher dans `memory/` et `transcripts/`.

## Scope MVP

- Backend: BM25 (Tantivy)
- Sources indexées:
  - `memory/*.md` et `memory/index-*.json` (boost élevé)
  - `transcripts/*.jsonl` (boost plus faible)
- Recherche:
  - requête texte
  - filtres optionnels (`kind`, `date_from`, `date_to`, `session`)
  - top-k résultats avec score + snippet

## Schéma de document indexé

```json
{
  "id": "transcript:2026-02-25:842",
  "kind": "transcript|memory",
  "source": "path",
  "session": "main",
  "ts": "2026-02-25T17:30:12Z",
  "title": "optional",
  "content": "...",
  "tags": ["user", "agent", "decision"],
  "stable": false
}
```

## Ranking recommandé

`final_score = bm25 * field_boost * recency_boost`

- field_boost:
  - `memory`: 1.6
  - `transcript`: 1.0
- recency_boost:
  - décroissance douce (7 jours)

## API interne visée

- `index build --full`
- `index update --since-cursor`
- `index search --q "..." --k 8 [--kind memory]`
- `index doctor`

## État/cursors

Fichier: `basedir/state/indexer-state.json`

- `last_indexed_ts`
- `last_transcript_offsets`
- `schema_version`

## Milestones

1. Parser sources + normalisation documents
2. Build index full
3. Search simple + snippets
4. Update incrémental via cursor
5. Intégration tool `memory_search_v2`
