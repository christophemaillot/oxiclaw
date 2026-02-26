use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED,
    STRING, TEXT,
};
use tantivy::{doc, Index, TantivyDocument, Term};

use rusqlite::{params, Connection};

const DEFAULT_HOT_DAYS: i64 = 2;
const RRF_K: f32 = 50.0;
const EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::AllMiniLML6V2;

fn vector_enabled() -> bool {
    env::var("OXICLAW_VECTOR_ENABLE")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

#[derive(Serialize, Deserialize, Clone)]
struct TranscriptLine {
    ts: String,
    session_id: String,
    role: String,
    content: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct IndexerState {
    last_file: Option<String>,
    last_line: usize,
    updated_at: String,
    last_memory_reindex_at: String,
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct Fields {
    id: tantivy::schema::Field,
    ts: tantivy::schema::Field,
    ts_epoch: tantivy::schema::Field,
    role: tantivy::schema::Field,
    session_id: tantivy::schema::Field,
    source_type: tantivy::schema::Field,
    content: tantivy::schema::Field,
    source_file: tantivy::schema::Field,
    source_line: tantivy::schema::Field,
    mention_count: tantivy::schema::Field,
}

#[derive(Clone)]
struct MemorySearchHit {
    id: String,
    ts: String,
    role: String,
    session_id: String,
    source_type: String,
    source_file: String,
    source_line: u64,
    mention_count: u64,
    ts_epoch: i64,
    content: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct VectorDoc {
    id: String,
    ts: String,
    ts_epoch: i64,
    role: String,
    session_id: String,
    source_type: String,
    source_file: String,
    source_line: u64,
    mention_count: u64,
    content: String,
    embedding: Vec<f32>,
}

pub struct MemorySearchOptions {
    pub limit: usize,
    pub archive: bool,
}

pub fn run_indexer_once(basedir: &Path) -> Result<()> {
    info!("indexer:start basedir={}", basedir.display());
    let _lock = match try_lock(basedir)? {
        Some(l) => l,
        None => {
            debug!("indexer:skip lock déjà pris basedir={}", basedir.display());
            return Ok(());
        }
    };

    let mut state = read_state(basedir)?;
    let transcript_files = list_transcript_files(basedir)?;

    let (index, fields) = open_or_create_tantivy_index(basedir)?;
    let mut writer = index.writer(50_000_000)?;
    let mut total_processed = 0usize;

    for path in transcript_files {
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };

        let start_line = if state.last_file.as_deref() == Some(&file_name) {
            state.last_line + 1
        } else if state
            .last_file
            .as_ref()
            .map(|f| f.as_str() < file_name.as_str())
            .unwrap_or(true)
        {
            1
        } else {
            continue;
        };

        debug!(
            "indexer:file source={} start_line={}",
            path.display(),
            start_line
        );

        let processed = index_transcript_file(&mut writer, &fields, &path, start_line)?;
        total_processed += processed;
        if processed > 0 {
            info!(
                "indexer:file_done file={} added_docs={} line_range={}..{}",
                file_name,
                processed,
                start_line,
                start_line + processed - 1
            );
            state.last_file = Some(file_name);
            state.last_line = start_line + processed - 1;
            state.updated_at = Utc::now().to_rfc3339();
        }
    }

    let memory_docs = index_memory_markdown_files(&mut writer, &fields, basedir)?;
    total_processed += memory_docs;
    if memory_docs > 0 {
        info!("indexer:memory_done added_docs={}", memory_docs);
    }
    state.last_memory_reindex_at = Utc::now().to_rfc3339();

    writer.commit()?;

    // Sprint B: index vectoriel local (embeddings ORT via fastembed + stockage SQLite)
    if vector_enabled() {
        if let Err(e) = rebuild_vector_index_lancedb(basedir) {
            warn!("vector_index_sqlite: skip ({e})");
        }
        if let Err(e) = rebuild_vector_index(basedir) {
            warn!("vector_index_json_fallback: skip ({e})");
        }
    } else {
        info!("vector_index: disabled (set OXICLAW_VECTOR_ENABLE=1 to enable)");
    }

    write_state(basedir, &state)?;
    info!(
        "indexer:done basedir={} total_added_docs={} cursor={:?}:{}",
        basedir.display(),
        total_processed,
        state.last_file,
        state.last_line
    );
    Ok(())
}

pub fn memory_search(
    basedir: &PathBuf,
    query: &str,
    options: MemorySearchOptions,
) -> Result<Vec<String>> {
    info!(
        "memory_search:start query='{}' limit={} archive={}",
        query, options.limit, options.archive
    );
    let (index, fields) = open_or_create_tantivy_index(basedir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();

    let parser = QueryParser::for_index(&index, vec![fields.content, fields.role]);
    let text_query = parser
        .parse_query_lenient(query)
        .0;

    let query_box: Box<dyn Query> = if options.archive {
        Box::new(BooleanQuery::new(vec![(Occur::Must, text_query)]))
    } else {
        let all = Box::new(AllQuery);
        let archive_term = Term::from_field_text(fields.source_type, "transcript_archive");
        let archive_q = Box::new(TermQuery::new(archive_term, IndexRecordOption::Basic));

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![
            (Occur::Must, text_query),
            (Occur::Must, all),
            (Occur::MustNot, archive_q),
        ];

        if let Some(current_sid) = current_session_id(basedir) {
            let sid_term = Term::from_field_text(fields.session_id, &current_sid);
            let sid_q = Box::new(TermQuery::new(sid_term, IndexRecordOption::Basic));
            clauses.push((Occur::MustNot, sid_q));
        }

        Box::new(BooleanQuery::new(clauses))
    };

    let fetch_limit = (options.limit.max(1) * 10).min(200);
    let top_docs = searcher.search(&query_box, &TopDocs::with_limit(fetch_limit))?;

    let mut lexical_ranks: Vec<String> = Vec::new();
    let mut hits_by_id: HashMap<String, MemorySearchHit> = HashMap::new();

    for (_score, addr) in top_docs {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let hit = doc_to_hit(&doc, &fields);
        lexical_ranks.push(hit.id.clone());
        hits_by_id.insert(hit.id.clone(), hit);
    }
    debug!(
        "memory_search:lexical_hits={} fetch_limit={}",
        lexical_ranks.len(),
        fetch_limit
    );

    let (vector_ranks, vector_hits) = vector_search(basedir, query, fetch_limit, options.archive);
    debug!("memory_search:vector_hits={}", vector_ranks.len());
    for (id, hit) in vector_hits {
        hits_by_id.entry(id).or_insert(hit);
    }

    let mut lexical_pos: HashMap<String, usize> = HashMap::new();
    for (i, id) in lexical_ranks.iter().enumerate() {
        lexical_pos.insert(id.clone(), i + 1);
    }

    let mut vector_pos: HashMap<String, usize> = HashMap::new();
    for (i, id) in vector_ranks.iter().enumerate() {
        vector_pos.insert(id.clone(), i + 1);
    }

    let mut reranked: Vec<(f32, String)> = Vec::new();
    let mut scored_debug: Vec<(f32, String, Option<usize>, Option<usize>, String)> = Vec::new();
    for (id, hit) in hits_by_id {
        let mut rrf = 0.0_f32;
        if let Some(rank) = lexical_pos.get(&id) {
            rrf += 1.0 / (RRF_K + *rank as f32);
        }
        if let Some(rank) = vector_pos.get(&id) {
            rrf += 1.0 / (RRF_K + *rank as f32);
        }

        let recency = recency_score(hit.ts_epoch, &hit.source_type);
        let repeat = repeat_score(hit.mention_count);
        let stability = if hit.source_type == "memory" { 1.0 } else { 0.3 };
        let archive_penalty = if hit.source_type == "transcript_archive" { 0.2 } else { 0.0 };

        let final_score = 0.55 * rrf + 0.20 * recency + 0.15 * repeat + 0.10 * stability - archive_penalty;

        let snippet = hit.content.chars().take(180).collect::<String>();
        let row = format!(
            "[{}] [{}] ({}) [{}:{}] [mentions={}] {}",
            hit.id,
            hit.ts,
            hit.role,
            hit.source_file,
            hit.source_line,
            hit.mention_count,
            snippet
        );

        scored_debug.push((
            final_score,
            id.clone(),
            lexical_pos.get(&id).copied(),
            vector_pos.get(&id).copied(),
            hit.source_type.clone(),
        ));
        reranked.push((final_score, row));
    }

    reranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    scored_debug.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));

    for (i, (score, id, lrank, vrank, stype)) in scored_debug.iter().take(5).enumerate() {
        debug!(
            "memory_search:rank#{} id={} score={:.4} lexical_rank={:?} vector_rank={:?} source_type={}",
            i + 1,
            id,
            score,
            lrank,
            vrank,
            stype
        );
    }

    let out: Vec<String> = reranked
        .into_iter()
        .take(options.limit.max(1))
        .map(|(_, row)| row)
        .collect();

    info!("memory_search:done returned={}", out.len());
    Ok(out)
}

pub fn memory_get(basedir: &PathBuf, id: &str) -> Result<Option<String>> {
    debug!("memory_get id={}", id);
    let (index, fields) = open_or_create_tantivy_index(basedir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();

    let term = Term::from_field_text(fields.id, id);
    let q = TermQuery::new(term, IndexRecordOption::Basic);
    let docs = searcher.search(&q, &TopDocs::with_limit(1))?;

    if let Some((_score, addr)) = docs.into_iter().next() {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let hit = doc_to_hit(&doc, &fields);
        return Ok(Some(format!(
            "[{}] [{}] ({}) [{}:{}] [mentions={}] {}",
            hit.id,
            hit.ts,
            hit.role,
            hit.source_file,
            hit.source_line,
            hit.mention_count,
            hit.content
        )));
    }

    Ok(None)
}

fn index_transcript_file(
    writer: &mut tantivy::IndexWriter,
    fields: &Fields,
    transcript_path: &Path,
    start_line: usize,
) -> Result<usize> {
    let source_file = match transcript_path.file_name().and_then(|n| n.to_str()) {
        Some(v) => v.to_string(),
        None => return Ok(0),
    };

    let file = File::open(transcript_path)?;
    let reader = BufReader::new(file);

    let mut processed = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line_no = i + 1;
        if line_no < start_line {
            continue;
        }

        let raw = match line {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parsed: TranscriptLine = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = stable_entry_id(&source_file, line_no, &parsed.content);
        let ts_epoch = parse_ts_epoch(&parsed.ts);

        writer.delete_term(Term::from_field_text(fields.id, &id));
        writer.add_document(doc!(
            fields.id => id,
            fields.ts => parsed.ts,
            fields.ts_epoch => ts_epoch,
            fields.role => parsed.role,
            fields.session_id => parsed.session_id,
            fields.source_type => infer_source_type(&source_file, ts_epoch),
            fields.content => parsed.content,
            fields.source_file => source_file.clone(),
            fields.source_line => line_no as u64,
            fields.mention_count => 1u64,
        ))?;

        processed += 1;
    }

    Ok(processed)
}

fn open_or_create_tantivy_index(basedir: &Path) -> Result<(Index, Fields)> {
    let index_dir = basedir.join("memory").join("tantivy");
    fs::create_dir_all(&index_dir)?;

    let schema = build_schema();

    let index = match Index::open_in_dir(&index_dir) {
        Ok(idx) => {
            if fields_from_schema(idx.schema()).is_err() {
                warn!(
                    "index schema mismatch, recreating tantivy index at {}",
                    index_dir.display()
                );
                fs::remove_dir_all(&index_dir)?;
                fs::create_dir_all(&index_dir)?;
                Index::create_in_dir(&index_dir, schema.clone())?
            } else {
                idx
            }
        }
        Err(_) => Index::create_in_dir(&index_dir, schema.clone())?,
    };

    let fields = fields_from_schema(index.schema())?;
    Ok((index, fields))
}

fn build_schema() -> Schema {
    let mut schema_builder = Schema::builder();

    let role_options = TextOptions::default()
        .set_stored()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("raw").set_index_option(
            IndexRecordOption::Basic,
        ));
    let source_type_options = role_options.clone();

    let ts_epoch_options = NumericOptions::default().set_stored().set_fast();
    let mention_count_options = NumericOptions::default().set_stored().set_fast();

    schema_builder.add_text_field("id", STRING | STORED);
    schema_builder.add_text_field("ts", STRING | STORED);
    schema_builder.add_i64_field("ts_epoch", ts_epoch_options);
    schema_builder.add_text_field("role", role_options);
    schema_builder.add_text_field("session_id", STRING | STORED);
    schema_builder.add_text_field("source_type", source_type_options);
    schema_builder.add_text_field("content", TEXT | STORED);
    schema_builder.add_text_field("source_file", STRING | STORED);
    schema_builder.add_u64_field("source_line", STORED | FAST);
    schema_builder.add_u64_field("mention_count", mention_count_options);
    schema_builder.build()
}

fn fields_from_schema(schema: Schema) -> Result<Fields> {
    let id = schema
        .get_field("id")
        .map_err(|_| anyhow::anyhow!("champ 'id' absent du schema Tantivy"))?;
    let ts = schema
        .get_field("ts")
        .map_err(|_| anyhow::anyhow!("champ 'ts' absent du schema Tantivy"))?;
    let ts_epoch = schema
        .get_field("ts_epoch")
        .map_err(|_| anyhow::anyhow!("champ 'ts_epoch' absent du schema Tantivy"))?;
    let role = schema
        .get_field("role")
        .map_err(|_| anyhow::anyhow!("champ 'role' absent du schema Tantivy"))?;
    let session_id = schema
        .get_field("session_id")
        .map_err(|_| anyhow::anyhow!("champ 'session_id' absent du schema Tantivy"))?;
    let source_type = schema
        .get_field("source_type")
        .map_err(|_| anyhow::anyhow!("champ 'source_type' absent du schema Tantivy"))?;
    let content = schema
        .get_field("content")
        .map_err(|_| anyhow::anyhow!("champ 'content' absent du schema Tantivy"))?;
    let source_file = schema
        .get_field("source_file")
        .map_err(|_| anyhow::anyhow!("champ 'source_file' absent du schema Tantivy"))?;
    let source_line = schema
        .get_field("source_line")
        .map_err(|_| anyhow::anyhow!("champ 'source_line' absent du schema Tantivy"))?;
    let mention_count = schema
        .get_field("mention_count")
        .map_err(|_| anyhow::anyhow!("champ 'mention_count' absent du schema Tantivy"))?;

    Ok(Fields {
        id,
        ts,
        ts_epoch,
        role,
        session_id,
        source_type,
        content,
        source_file,
        source_line,
        mention_count,
    })
}

fn doc_to_hit(doc: &TantivyDocument, fields: &Fields) -> MemorySearchHit {
    let id = doc
        .get_first(fields.id)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let ts = doc
        .get_first(fields.ts)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let role = doc
        .get_first(fields.role)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let session_id = doc
        .get_first(fields.session_id)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let source_type = doc
        .get_first(fields.source_type)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let source_file = doc
        .get_first(fields.source_file)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let source_line = doc
        .get_first(fields.source_line)
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mention_count = doc
        .get_first(fields.mention_count)
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let ts_epoch = doc
        .get_first(fields.ts_epoch)
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| Utc::now().timestamp());
    let content = doc
        .get_first(fields.content)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    MemorySearchHit {
        id,
        ts,
        role,
        session_id,
        source_type,
        source_file,
        source_line,
        mention_count,
        ts_epoch,
        content,
    }
}

fn current_session_id(basedir: &Path) -> Option<String> {
    let files = list_transcript_files(basedir).ok()?;
    let latest = files.last()?;
    let file = File::open(latest).ok()?;
    let reader = BufReader::new(file);
    let mut last = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() { continue; }
        last = Some(line);
    }
    let raw = last?;
    let parsed: TranscriptLine = serde_json::from_str(&raw).ok()?;
    Some(parsed.session_id)
}

fn list_transcript_files(basedir: &Path) -> Result<Vec<PathBuf>> {
    let dir = basedir.join("transcripts");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with("session-") && name.ends_with(".jsonl") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn list_memory_files(basedir: &Path) -> Result<Vec<PathBuf>> {
    let dir = basedir.join("memory");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with("MEMORY-") && name.ends_with(".md") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn index_memory_markdown_files(
    writer: &mut tantivy::IndexWriter,
    fields: &Fields,
    basedir: &Path,
) -> Result<usize> {
    let mut processed = 0usize;

    let files = list_memory_files(basedir)?;
    let mut normalized_counts: HashMap<String, u64> = HashMap::new();
    for path in &files {
        let raw = fs::read_to_string(path).unwrap_or_default();
        for line in raw.lines() {
            let content = line.trim();
            if content.is_empty() || !content.starts_with('-') {
                continue;
            }
            let normalized = normalize_for_hash(content);
            *normalized_counts.entry(normalized).or_insert(0) += 1;
        }
    }

    for path in files {
        let source_file = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };
        debug!("indexer:memory_file source={}", path.display());

        let raw = fs::read_to_string(&path).unwrap_or_default();
        for (i, line) in raw.lines().enumerate() {
            let line_no = i + 1;
            let content = line.trim();
            if content.is_empty() || !content.starts_with('-') {
                continue;
            }

            let id = stable_memory_line_id(&source_file, line_no);
            let normalized = normalize_for_hash(content);
            let mention_count = normalized_counts.get(&normalized).copied().unwrap_or(1);
            writer.delete_term(Term::from_field_text(fields.id, &id));
            writer.add_document(doc!(
                fields.id => id,
                fields.ts => Utc::now().to_rfc3339(),
                fields.ts_epoch => Utc::now().timestamp(),
                fields.role => "memory",
                fields.session_id => "memory",
                fields.source_type => "memory",
                fields.content => content.to_string(),
                fields.source_file => source_file.clone(),
                fields.source_line => line_no as u64,
                fields.mention_count => mention_count,
            ))?;
            processed += 1;
        }
    }

    Ok(processed)
}

fn collect_vector_docs(basedir: &Path) -> Result<Vec<MemorySearchHit>> {
    let mut out = Vec::new();

    for path in list_transcript_files(basedir)? {
        let source_file = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        for (i, line) in reader.lines().enumerate() {
            let line_no = i + 1;
            let raw = match line {
                Ok(v) => v,
                Err(_) => continue,
            };
            let parsed: TranscriptLine = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ts_epoch = parse_ts_epoch(&parsed.ts);
            out.push(MemorySearchHit {
                id: stable_entry_id(&source_file, line_no, &parsed.content),
                ts: parsed.ts,
                role: parsed.role,
                session_id: parsed.session_id,
                source_type: infer_source_type(&source_file, ts_epoch).to_string(),
                source_file: source_file.clone(),
                source_line: line_no as u64,
                mention_count: 1,
                ts_epoch,
                content: parsed.content,
            });
        }
    }

    let files = list_memory_files(basedir)?;
    let mut normalized_counts: HashMap<String, u64> = HashMap::new();
    for path in &files {
        let raw = fs::read_to_string(path).unwrap_or_default();
        for line in raw.lines() {
            let content = line.trim();
            if content.is_empty() || !content.starts_with('-') {
                continue;
            }
            let normalized = normalize_for_hash(content);
            *normalized_counts.entry(normalized).or_insert(0) += 1;
        }
    }

    for path in files {
        let source_file = match path.file_name().and_then(|n| n.to_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };
        let raw = fs::read_to_string(&path).unwrap_or_default();
        for (i, line) in raw.lines().enumerate() {
            let line_no = i + 1;
            let content = line.trim();
            if content.is_empty() || !content.starts_with('-') {
                continue;
            }
            let normalized = normalize_for_hash(content);
            out.push(MemorySearchHit {
                id: stable_memory_line_id(&source_file, line_no),
                ts: Utc::now().to_rfc3339(),
                role: "memory".to_string(),
                session_id: "memory".to_string(),
                source_type: "memory".to_string(),
                source_file: source_file.clone(),
                source_line: line_no as u64,
                mention_count: normalized_counts.get(&normalized).copied().unwrap_or(1),
                ts_epoch: Utc::now().timestamp(),
                content: content.to_string(),
            });
        }
    }

    Ok(out)
}

fn vector_index_path(basedir: &Path) -> PathBuf {
    basedir.join("memory").join("vector_index.json")
}

fn rebuild_vector_index(basedir: &Path) -> Result<()> {
    let docs = collect_vector_docs(basedir)?;
    if docs.is_empty() {
        return Ok(());
    }

    let model = embedding_model_for_basedir(basedir)?;
    let inputs: Vec<String> = docs.iter().map(|d| d.content.clone()).collect();
    let embeddings = model.embed(inputs, None)?;

    let mut out = Vec::with_capacity(docs.len());
    for (doc, emb) in docs.into_iter().zip(embeddings.into_iter()) {
        out.push(VectorDoc {
            id: doc.id,
            ts: doc.ts,
            ts_epoch: doc.ts_epoch,
            role: doc.role,
            session_id: doc.session_id,
            source_type: doc.source_type,
            source_file: doc.source_file,
            source_line: doc.source_line,
            mention_count: doc.mention_count,
            content: doc.content,
            embedding: emb,
        });
    }

    let path = vector_index_path(basedir);
    fs::write(&path, serde_json::to_vec(&out)?)?;
    info!("vector_index: rebuilt docs={} path={}", out.len(), path.display());
    Ok(())
}

fn vector_search(
    basedir: &Path,
    query: &str,
    limit: usize,
    archive: bool,
) -> (Vec<String>, HashMap<String, MemorySearchHit>) {
    if !vector_enabled() {
        debug!("vector_search:disabled");
        return (Vec::new(), HashMap::new());
    }
    match vector_search_lancedb(basedir, query, limit, archive) {
        Ok((r, h)) if !r.is_empty() => {
            debug!("vector_search:backend=lancedb hits={}", r.len());
            (r, h)
        }
        Ok((_r, _h)) => {
            debug!("vector_search:backend=lancedb hits=0 fallback=json");
            vector_search_json_fallback(basedir, query, limit, archive)
        }
        Err(e) => {
            debug!("vector_search:backend=lancedb error='{}' fallback=json", e);
            vector_search_json_fallback(basedir, query, limit, archive)
        }
    }
}

fn vector_search_json_fallback(
    basedir: &Path,
    query: &str,
    limit: usize,
    archive: bool,
) -> (Vec<String>, HashMap<String, MemorySearchHit>) {
    let path = vector_index_path(basedir);
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), HashMap::new()),
    };

    let docs: Vec<VectorDoc> = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), HashMap::new()),
    };

    let model = match embedding_model() {
        Ok(m) => m,
        Err(_) => return (Vec::new(), HashMap::new()),
    };

    let q_emb = match model.embed(vec![query.to_string()], None) {
        Ok(v) if !v.is_empty() => v[0].clone(),
        _ => return (Vec::new(), HashMap::new()),
    };

    let mut scored: Vec<(f32, VectorDoc)> = docs
        .into_iter()
        .filter(|d| archive || d.source_type != "transcript_archive")
        .filter_map(|d| {
            let sim = cosine_similarity(&q_emb, &d.embedding)?;
            Some((sim, d))
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));

    let mut ranks = Vec::new();
    let mut hits = HashMap::new();
    for (_sim, d) in scored.into_iter().take(limit.max(1)) {
        ranks.push(d.id.clone());
        hits.insert(
            d.id.clone(),
            MemorySearchHit {
                id: d.id,
                ts: d.ts,
                role: d.role,
                session_id: d.session_id,
                source_type: d.source_type,
                source_file: d.source_file,
                source_line: d.source_line,
                mention_count: d.mention_count,
                ts_epoch: d.ts_epoch,
                content: d.content,
            },
        );
    }

    (ranks, hits)
}

fn embedding_model_for_basedir(basedir: &Path) -> Result<TextEmbedding> {
    let cache_dir = basedir.join("models").join("fastembed");
    fs::create_dir_all(&cache_dir)?;
    let init = InitOptions::new(EMBEDDING_MODEL).with_cache_dir(cache_dir);
    Ok(TextEmbedding::try_new(init)?)
}

pub fn warmup_embeddings(basedir: &Path) -> Result<()> {
    let model = embedding_model_for_basedir(basedir)?;
    let _ = model.embed(vec!["warmup".to_string()], None)?;
    info!("embedding:warmup ok cache_dir={}", basedir.join("models/fastembed").display());
    Ok(())
}

fn embedding_model() -> Result<TextEmbedding> {
    let init = InitOptions::new(EMBEDDING_MODEL);
    Ok(TextEmbedding::try_new(init)?)
}

fn sqlite_vec_path(basedir: &Path) -> PathBuf {
    basedir.join("memory").join("sqlite_vec.db")
}

fn rebuild_vector_index_lancedb(basedir: &Path) -> Result<()> {
    let docs = collect_vector_docs(basedir)?;
    if docs.is_empty() {
        return Ok(());
    }

    let model = embedding_model_for_basedir(basedir)?;
    let inputs: Vec<String> = docs.iter().map(|d| d.content.clone()).collect();
    let embeddings = model.embed(inputs, None)?;

    let db_path = sqlite_vec_path(basedir);
    let conn = Connection::open(db_path.clone())?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        DROP TABLE IF EXISTS memory_vectors;
        CREATE TABLE memory_vectors (
            id TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            ts_epoch INTEGER NOT NULL,
            role TEXT NOT NULL,
            session_id TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_file TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            mention_count INTEGER NOT NULL,
            content TEXT NOT NULL,
            vector_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memory_vectors_source_type ON memory_vectors(source_type);
        ",
    )?;

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO memory_vectors(id, ts, ts_epoch, role, session_id, source_type, source_file, source_line, mention_count, content, vector_json)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for (doc, emb) in docs.into_iter().zip(embeddings.into_iter()) {
            let vector_json = serde_json::to_string(&emb)?;
            stmt.execute(params![
                doc.id,
                doc.ts,
                doc.ts_epoch,
                doc.role,
                doc.session_id,
                doc.source_type,
                doc.source_file,
                doc.source_line,
                doc.mention_count,
                doc.content,
                vector_json,
            ])?;
        }
    }
    tx.commit()?;

    info!("vector_index_sqlite: rebuilt path={}", db_path.display());
    Ok(())
}

fn vector_search_lancedb(
    basedir: &Path,
    query: &str,
    limit: usize,
    archive: bool,
) -> Result<(Vec<String>, HashMap<String, MemorySearchHit>)> {
    let model = embedding_model_for_basedir(basedir)?;
    let q_emb = model
        .embed(vec![query.to_string()], None)?
        .into_iter()
        .next()
        .unwrap_or_default();
    if q_emb.is_empty() {
        return Ok((Vec::new(), HashMap::new()));
    }

    let conn = Connection::open(sqlite_vec_path(basedir))?;
    let mut stmt = conn.prepare(
        "SELECT id, ts, ts_epoch, role, session_id, source_type, source_file, source_line, mention_count, content, vector_json
         FROM memory_vectors",
    )?;

    let rows = stmt.query_map([], |row| {
        let vector_json: String = row.get(10)?;
        let emb: Vec<f32> = serde_json::from_str(&vector_json).unwrap_or_default();
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, u64>(7)?,
            row.get::<_, u64>(8)?,
            row.get::<_, String>(9)?,
            emb,
        ))
    })?;

    let mut scored: Vec<(f32, MemorySearchHit)> = Vec::new();
    for row in rows {
        let (id, ts, ts_epoch, role, session_id, source_type, source_file, source_line, mention_count, content, emb) = row?;
        if !archive && source_type == "transcript_archive" {
            continue;
        }
        if let Some(sim) = cosine_similarity(&q_emb, &emb) {
            scored.push((
                sim,
                MemorySearchHit {
                    id,
                    ts,
                    role,
                    session_id,
                    source_type,
                    source_file,
                    source_line,
                    mention_count,
                    ts_epoch,
                    content,
                },
            ));
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));

    let mut ranks = Vec::new();
    let mut hits = HashMap::new();
    for (_sim, hit) in scored.into_iter().take(limit.max(1) * 3) {
        ranks.push(hit.id.clone());
        hits.insert(hit.id.clone(), hit);
    }

    Ok((ranks, hits))
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

fn state_path(basedir: &Path) -> PathBuf {
    basedir.join("state").join("indexer_state.json")
}

fn lock_path(basedir: &Path) -> PathBuf {
    basedir.join("state").join("indexer.lock")
}

fn try_lock(basedir: &Path) -> Result<Option<LockGuard>> {
    let path = lock_path(basedir);
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "pid:{} ts:{}", std::process::id(), Utc::now().to_rfc3339());
            Ok(Some(LockGuard { path }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn read_state(basedir: &Path) -> Result<IndexerState> {
    let path = state_path(basedir);
    if !path.exists() {
        return Ok(IndexerState::default());
    }
    let txt = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&txt).unwrap_or_default())
}

fn write_state(basedir: &Path, state: &IndexerState) -> Result<()> {
    let path = state_path(basedir);
    fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn stable_entry_id(source_file: &str, line_no: usize, content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source_file.hash(&mut hasher);
    line_no.hash(&mut hasher);
    normalize_for_hash(content).hash(&mut hasher);
    format!("mem-{line_no}-{:016x}", hasher.finish())
}

fn stable_memory_line_id(source_file: &str, line_no: usize) -> String {
    let mut hasher = DefaultHasher::new();
    source_file.hash(&mut hasher);
    line_no.hash(&mut hasher);
    format!("mline-{line_no}-{:016x}", hasher.finish())
}

fn normalize_for_hash(input: &str) -> String {
    input
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn parse_ts_epoch(ts: &str) -> i64 {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| Utc::now().timestamp())
}

fn infer_source_type(source_file: &str, ts_epoch: i64) -> &'static str {
    if source_file.starts_with("MEMORY-") {
        return "memory";
    }
    let age = Utc::now().timestamp() - ts_epoch;
    let hot_limit = Duration::days(DEFAULT_HOT_DAYS).num_seconds();
    if age <= hot_limit {
        "transcript_hot"
    } else {
        "transcript_archive"
    }
}

fn recency_score(ts_epoch: i64, source_type: &str) -> f32 {
    let age_hours = ((Utc::now().timestamp() - ts_epoch).max(0) as f32) / 3600.0;
    let half_life = if source_type == "memory" { 24.0 * 14.0 } else { 24.0 * 2.0 };
    2f32.powf(-age_hours / half_life)
}

fn repeat_score(mention_count: u64) -> f32 {
    let cap = 10.0_f32;
    let m = mention_count as f32;
    ((1.0_f32 + m).ln() / (1.0_f32 + cap).ln()).clamp(0.0, 1.0)
}
