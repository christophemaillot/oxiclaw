# Griffe — Résumé détaillé de la session (2026-02-25)

## 1) Objectif de départ
Christophe voulait recoder un "genre d'OpenClaw" en Rust, en mode pédagogique:
- comprendre le fonctionnement d'un agent,
- commencer simple (boucle terminal + LLM),
- itérer vers une architecture propre (tools, session, mémoire, runtime),
- garder de la visibilité (debug/logs),
- préparer des transports externes (HTTP puis Telegram).

---

## 2) Bootstrapping initial
Projet créé dans:
- `/home/pax/.openclaw/workspace/griffe`

Fichiers initiaux:
- `Cargo.toml`
- `src/main.rs`
- `README.md`
- `.gitignore`

Première version:
- boucle terminal (`stdin -> LLM -> stdout`),
- commandes basiques (`/help`, `/reset`, `/quit`),
- prompt système minimal.

---

## 3) Connexion LLM
Deux modes ont été traités:

### A. Via OpenClaw Gateway (OAuth existant)
- tentative de réutilisation du setup OAuth OpenClaw,
- compréhension: pas de clé OpenAI récupérable directement (profil OAuth côté OpenClaw).

### B. Provider direct (retenu ensuite, ex Kimi)
- variables `OPENAI_*` (compat OpenAI API),
- support endpoint configurable (`OPENAI_BASE_URL`),
- support modèle configurable (`OPENAI_MODEL`),
- clé via `OPENAI_API_KEY`.

---

## 4) Premiers tools simples
Ajout initial de tools locaux:
- `time`
- `echo`

Au début:
- déclenchement manuel via commandes `/tool ...`,
- puis passage à un protocole où le LLM peut demander un tool.

---

## 5) Protocole tool-calling et stabilisation

### Problème rencontré
Le modèle produisait parfois des formats non conformes (ex: `{"type":"echo"...}`) ou bouclait sur des tool calls.

### Corrections appliquées
1. Protocole explicite dans le system prompt.
2. Parseur d'actions (`tool_call` / `final_answer`).
3. Boucle de réparation en cas de format invalide (`PROTOCOLE_INVALIDE...`).
4. Limite de steps (`max_steps`) pour éviter les boucles infinies.
5. Fallback sur dernier résultat tool si limite atteinte.

### Résultat
Le turn agent est devenu stable et explicable:
- LLM -> action structurée -> exécution tool -> résultat -> finalisation.

---

## 6) Refactor architecture (modulaire)
Le code a été découpé en modules:

- `src/config.rs` : config runtime/env/args
- `src/llm/` : abstraction `LlmClient` + impl OpenAI-compatible
- `src/tools/mod.rs` : trait Tool, registry, specs + parseur d'actions
- `src/session.rs` : état conversationnel
- `src/engine.rs` : logique d'un turn agent
- `src/storage.rs` : transcripts + index mémoire
- `src/persona.rs` : construction du system prompt depuis fichiers persona
- `src/runtime.rs` : cœur applicatif (indépendant du transport)
- `src/http.rs` : transport HTTP (axum)
- `src/main.rs` : bootstrap + transport CLI/HTTP

---

## 7) Debugabilité
Ajout de logs détaillés moteur via:
- `GRIFFE_DEBUG=1`

Logs utiles:
- début/fin de turn,
- step courant,
- sortie modèle (tronquée),
- action parsée (`tool_call`, `final_answer`, `parse_error`),
- résultat tool,
- fallback/erreur.

---

## 8) Basedir + layout runtime
Ajout du support:
- `--basedir <path>`
- fallback env: `GRIFFE_BASEDIR`

Layout créé automatiquement:
- `conf/`
- `memory/`
- `transcripts/`
- `state/`
- `logs/`
- `SOUL.md`
- `IDENTITY.md`
- `USER.md`

---

## 9) Persona injectée dans le prompt
Constat validé: les fichiers persona ne sont pas utilisés automatiquement tant qu'on ne les charge pas.

Implémentation:
- lecture de `SOUL.md`, `IDENTITY.md`, `USER.md` dans `persona.rs`,
- injection dans le system prompt,
- commande `/reload-persona` pour recharger sans restart.

`Session` a été étendue avec:
- `set_system_prompt(...)`

Fichiers persona enrichis également dans:
- `griffe/basedir/SOUL.md`
- `griffe/basedir/IDENTITY.md`
- `griffe/basedir/USER.md`

---

## 10) Transcripts + mémoire locale

### Transcripts
Append automatique en JSONL:
- rôle user / assistant / system / error,
- timestamp UTC.

### Chronodate des fichiers
Passage à des fichiers journaliers:
- `transcripts/session-YYYY-MM-DD.jsonl`
- `memory/index-YYYY-MM-DD.json`

### Index mémoire (v1)
Mise à jour incrémentale:
- `entries[]` (avec `id`, `ts`, `role`, `content`, `keywords`)
- `by_keyword` (comptage simple)

---

## 11) Tools mémoire
Ajout de:
- `memory_search(query, limit)`
- `memory_get(id)`

Flux:
1. `memory_search` retourne des lignes avec id (ex `index-2026-02-25.json#3`)
2. `memory_get` lit précisément l'entrée correspondante.

---

## 12) Séparation Core vs Transport
Refactor important:
- création de `AgentRuntime` (`src/runtime.rs`) comme cœur unique,
- CLI branché dessus,
- préparation pour transports additionnels.

---

## 13) Ajout d'un transport HTTP Rust
Lib utilisée:
- `axum`

Nouveau module:
- `src/http.rs`

Endpoints:
- `GET /health`
- `POST /chat` (`{"message":"..."}`)

Lancement HTTP:
- `cargo run -- --basedir ./basedir --http`

---

## 14) Fichier de configuration runtime
Ajout de:
- `basedir/conf/config.json`

Contenu prévu:
- `llm.api_key`, `llm.model`, `llm.base_url`
- `http.host`, `http.port`
- `telegram.enabled`, `telegram.bot_token`, `telegram.default_chat_id`

Stratégie de résolution:
- env vars prioritaire,
- sinon fallback sur `conf/config.json`.

---

## 15) Fichiers modifiés/ajoutés majeurs

### Créés ou refactorés
- `src/config.rs`
- `src/main.rs`
- `src/engine.rs`
- `src/session.rs`
- `src/tools/mod.rs`
- `src/storage.rs`
- `src/persona.rs`
- `src/runtime.rs`
- `src/http.rs`
- `README.md`
- `Cargo.toml` (ajout `axum`)

### Fichiers persona enrichis dans basedir
- `basedir/SOUL.md`
- `basedir/IDENTITY.md`
- `basedir/USER.md`

---

## 16) Commandes utiles récap

CLI:
```bash
cargo run -- --basedir ./basedir
```

CLI debug:
```bash
GRIFFE_DEBUG=1 cargo run -- --basedir ./basedir
```

HTTP:
```bash
cargo run -- --basedir ./basedir --http
```

Test HTTP:
```bash
curl -s http://127.0.0.1:8787/health
curl -s http://127.0.0.1:8787/chat \
  -H 'content-type: application/json' \
  -d '{"message":"Salut Griffon"}'
```

---

## 17) Prochaine étape prévue
Christophe a demandé ensuite:
- implémenter le support Telegram.

La base est prête grâce à la séparation:
- `AgentRuntime` (core)
- transports (CLI, HTTP)
- futur transport Telegram à brancher au même runtime.

---

## 18) Suite de la session — décisions & pistes (soir)

### A. Vision produit / agentique
- Confirmation que le levier principal est **le contexte** (prompt/policy + outils + mémoire), plus que le modèle seul.
- Objectif: rester **générique** tout en améliorant la prise de décision spontanée.

### B. Modèle mémoire clarifié
- `SOUL.md` = **constitution immuable** (non modifiable par l’agent).
- `AGENT.md` = mémoire opérationnelle évolutive (règles/heuristiques).
- `USER.md` = faits utilisateurs stables.
- Distinction confirmée:
  - **Transcripts** = journal brut exhaustif
  - **Memory** = connaissance distillée durable

### C. Implémentations effectuées pendant cette session
1. **Transport Telegram**
   - long polling
   - présence `typing` via `sendChatAction`
   - support `/start`, `/help`, `/reset`, `/reload-persona`
   - normalisation commandes `/<cmd>@BotName`
   - filtrage `default_chat_id`
   - message de bienvenue au premier message d’un chat (sur la durée de vie du process)

2. **Tool HTTP générique**
   - `http_request` ajouté (méthode/url/headers/query/json/body/timeout/max_chars)
   - bug runtime corrigé: abandon de `reqwest::blocking` (panic tokio) au profit de `ureq`

3. **Écriture mémoire contrôlée**
   - ajout de `AGENT.md`
   - ajout du tool `info_append(target,text)`
   - targets autorisées: `agent|user`
   - `soul` explicitement refusé
   - garde-fous: anti-doublon simple, taille max, blocage termes sensibles

4. **Prompt/policy enrichis**
   - inclusion de `AGENT.md` dans le system prompt
   - règles explicites de décision:
     - agir plutôt que bloquer
     - chercher en mémoire avant de poser une question
     - 1 question de clarification courte si nécessaire

### D. Roadmap évoquée (à prioriser)
- Skills
- Sous-agents (style OpenClaw)
- Injection auto de micro-contexte (heure courante, etc.)
- Scheduler cron/heartbeat-like (en cohérence avec licron)
- Outils d’écriture/modification de fichiers (sandbox par défaut)
- Tool `exec` puissant mais contraint (équilibre sécurité/capacités)

### E. Curator & indexation (orientation actuelle)
- Curator imaginé comme **sous-agent spécialisé** (LLM) pour distiller les transcripts.
- Principes retenus:
  - lock d’exécution
  - cursor temporel/offset
  - lecture fenêtrée + petite overlap
  - relecture mémoire existante avant append
  - idempotence/dédup
- Indexation:
  - BM25/Tantivy recommandé en premier (sans embeddings)
  - hybridation vectorielle potentielle plus tard

### F. Point d’architecture ouvert
- Hésitation sur un découpage en 3 process (`core/indexer/curator`).
- Option pragmatique proposée: **1 binaire modulaire** d’abord (3 modules internes), extraction en process séparés plus tard si nécessaire.

---

## 19) Clarification stratégique (2026-02-26) — mémoire, transcripts, retrieval

### A. Rôle de chaque couche (formalisation)
- **Transcripts** = journal brut, exhaustif, horodaté, vérifiable (source de vérité).
- **Memory distillée** = connaissance compacte et réutilisable (préférences, décisions, faits stables, récurrences).

Formule de référence:
- **Transcripts**: “ce qui s’est dit”
- **Memory**: “ce qu’il faut retenir pour mieux décider/agir”

### B. Injection contexte agent
Décision retenue: **ne pas injecter toute la mémoire en bloc** dans le prompt.

Approche recommandée par turn:
1. prompt système / policy
2. historique conversation récent
3. mémoire récupérée à la demande via `memory_search` (top-k court)

Objectif: maximiser le signal, limiter le bruit et les coûts tokens.

### C. Comportement cible de `memory_search`
Décision retenue:
- `memory_search` retourne en priorité des éléments issus de **memory distillée**
- + une **fenêtre de sécurité** de transcripts récents non encore distillés, pour ne rien rater

Priorisation souhaitée dans le ranking:
1. pertinence (match de la requête)
2. récence
3. récurrence (ce qui revient souvent)
4. stabilité/confiance de l’item

### D. Politique d’indexation transcripts (hot vs archive)
Décision retenue:
- **Hot index transcripts** (par défaut): 24–48h (jusqu’à 72h max selon charge)
- Au-delà: **désindexation du hot retrieval** (pas de suppression), conservation en archive

Important:
- On **désindexe pour le ranking par défaut**, on n’efface pas l’historique
- Les transcripts anciens restent disponibles pour audit/recherche explicite

### E. Option d’API proposée
Option simple validée pour démarrer:
- `archive=false` (défaut): memory distillée + transcripts hot
- `archive=true`: ajoute transcripts archive/cold (poids plus faible)

Évolution possible ultérieure:
- `scope=default|hot|memory|archive|all`

### F. Pourquoi ce schéma est jugé pertinent
- Réduit le bruit contextuel en inférence courante
- Force la distillation continue via curator
- Évite les oublis grâce à la fenêtre hot non distillée
- Préserve la traçabilité complète en archive

### G. Curator nightly (rappel)
Cadence confirmée: passage quotidien (soir) transcript → memory.
Principes à conserver:
- lock d’exécution
- cursor/offset
- overlap court
- idempotence + dédup
- relecture mémoire existante avant append

---

## 20) Implémentation Sprint A (prototype, sans rétrocompat index)

### A. `memory_search` — nouveau paramètre
- Ajout du paramètre booléen `archive` dans le tool.
- Comportement:
  - `archive=false` (défaut): exclut `transcript_archive` du retrieval courant.
  - `archive=true`: inclut aussi l’archive.

### B. Schéma Tantivy enrichi (recréation index autorisée)
Nouveaux champs indexés/stored:
- `ts_epoch` (i64 fast field)
- `source_type` (`memory` | `transcript_hot` | `transcript_archive`)
- `mention_count` (u64 fast field)

Champs conservés: `id`, `ts`, `role`, `content`, `source_file`, `source_line`.

Note prototype:
- si schéma incompatible détecté, l’index Tantivy est recréé automatiquement.

### C. Politique hot/archive appliquée
- Classification transcripts via âge (par défaut 2 jours):
  - <= 48h → `transcript_hot`
  - > 48h → `transcript_archive`
- Les données ne sont pas supprimées; seule la recherche par défaut filtre l’archive.

### D. Ranking hybride (dans l’app, après TopDocs Tantivy)
`final_score = 0.45*bm25 + 0.25*recency + 0.20*repeat + 0.10*stability - archive_penalty`

Détails:
- `bm25`: score lexical Tantivy normalisé (log)
- `recency`: décroissance exponentielle (half-life plus longue pour memory)
- `repeat`: `ln(1+mention_count)` borné [0..1]
- `stability`: bonus mémoire distillée vs transcript brut
- `archive_penalty`: malus léger pour `transcript_archive`

### E. `mention_count` (MVP)
- **Transcripts**: initialisé à `1` (événement brut).
- **Memory markdown**: comptage de répétition basé sur une normalisation textuelle simple
  (trim + lowercase + collapse espaces), puis agrégation des occurrences similaires
  sur l’ensemble des fichiers `memory/MEMORY-*.md`.

### F. Résultat attendu
- Recherche par défaut plus “utile” (mémoire distillée + signaux récents).
- Moins de bruit historique non distillé.
- Base prête pour Sprint B (hybride lexical + vectoriel type LanceDB/ORT).

### G. Outils de validation ajoutés (smoke tests)
Pour accélérer l’itération en prototype, ajout de 2 artefacts:

1. **Binaire de test** `src/bin/memory_probe.rs`
   - Permet de lancer `memory_search` hors boucle agent.
   - Paramètres:
     - `--basedir <path>`
     - `--query <text>`
     - `--limit <n>`
     - `--archive` (active `archive=true`)
     - `--reindex` (force un passage indexer avant requête)

2. **Script smoke** `scripts/smoke_memory_search.sh`
   - Exécute un mini scénario en 4 étapes:
     1) reindex,
     2) recherche par défaut (`archive=false`),
     3) même recherche avec archive,
     4) sonde de signal “répétition”.
   - But: comparer rapidement l’ordonnancement et les `mentions`.

Commande:
- `./scripts/smoke_memory_search.sh`
- ou `./scripts/smoke_memory_search.sh /chemin/vers/basedir`

### H. Décision de méthode validée
- En phase prototype: **pas de rétrocompat index** requise.
- Recréation/vidage d’index acceptée si changement de schéma.
- Priorité = vitesse d’itération + validation comportementale du ranking.

