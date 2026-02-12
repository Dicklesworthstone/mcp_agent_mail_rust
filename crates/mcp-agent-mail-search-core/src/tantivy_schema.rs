//! Tantivy schema definition, tokenizer chain, and schema versioning
//!
//! Defines the index schema for messages, agents, and projects with:
//! - Full-text fields (subject, body) with custom tokenizer chain
//! - Exact-match fields (sender, project, thread) for filtering
//! - Fast fields (timestamps, importance) for sorting and range queries
//! - Schema hash for automatic rebuild on schema changes

use sha2::{Digest, Sha256};
use tantivy::Index;
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, SchemaBuilder,
    TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};

/// Name of the custom tokenizer registered with Tantivy
pub const TOKENIZER_NAME: &str = "am_default";

/// Current schema version — bump when schema or tokenizer changes
const SCHEMA_VERSION: &str = "v1";

// ── Field handles ────────────────────────────────────────────────────────────

/// All field handles for the Agent Mail Tantivy index.
///
/// Obtain via [`build_schema()`] which returns both the `Schema` and these handles.
#[derive(Debug, Clone, Copy)]
pub struct FieldHandles {
    /// Document database ID (u64, indexed + stored + fast)
    pub id: Field,
    /// Document kind: "message", "agent", or "project" (string, indexed + stored + fast)
    pub doc_kind: Field,
    /// Subject/title (text, indexed + stored, boost 2.0x via query-time weighting)
    pub subject: Field,
    /// Body/content (text, indexed + stored, baseline boost)
    pub body: Field,
    /// Sender agent name (string, indexed + stored + fast)
    pub sender: Field,
    /// Project slug (string, indexed + stored + fast)
    pub project_slug: Field,
    /// Project ID (u64, indexed + stored + fast)
    pub project_id: Field,
    /// Thread ID (string, indexed + stored + fast)
    pub thread_id: Field,
    /// Importance level: low/normal/high/urgent (string, indexed + stored + fast)
    pub importance: Field,
    /// Created timestamp in microseconds since epoch (i64, indexed + fast)
    pub created_ts: Field,
    /// Program name for agents (string, stored)
    pub program: Field,
    /// Model name for agents (string, stored)
    pub model: Field,
}

// ── Schema construction ──────────────────────────────────────────────────────

/// Build the Tantivy schema and return field handles.
///
/// The schema is a unified index covering messages, agents, and projects.
/// The `doc_kind` field discriminates between document types at query time.
#[must_use]
pub fn build_schema() -> (Schema, FieldHandles) {
    let mut builder = SchemaBuilder::new();

    // Text field options with custom tokenizer + positions (for phrase queries)
    let text_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER_NAME)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );

    let text_stored = text_options | STORED;

    // ── Common fields ──
    let id = builder.add_u64_field("id", INDEXED | STORED | FAST);
    let doc_kind = builder.add_text_field("doc_kind", STRING | STORED | FAST);
    let project_id = builder.add_u64_field("project_id", INDEXED | STORED | FAST);
    let project_slug = builder.add_text_field("project_slug", STRING | STORED | FAST);
    let created_ts = builder.add_i64_field("created_ts", INDEXED | STORED | FAST);

    // ── Message fields ──
    let subject = builder.add_text_field("subject", text_stored.clone());
    let body = builder.add_text_field("body", text_stored);
    let sender = builder.add_text_field("sender", STRING | STORED | FAST);
    let thread_id = builder.add_text_field("thread_id", STRING | STORED | FAST);
    let importance = builder.add_text_field("importance", STRING | STORED | FAST);

    // ── Agent-specific fields (stored for display, not full-text indexed) ──
    let program = builder.add_text_field("program", STORED);
    let model = builder.add_text_field("model", STORED);

    let schema = builder.build();

    let handles = FieldHandles {
        id,
        doc_kind,
        subject,
        body,
        sender,
        project_slug,
        project_id,
        thread_id,
        importance,
        created_ts,
        program,
        model,
    };

    (schema, handles)
}

// ── Tokenizer registration ───────────────────────────────────────────────────

/// Register the custom `am_default` tokenizer with a Tantivy index.
///
/// Chain:
/// 1. `SimpleTokenizer` — splits on whitespace + punctuation
/// 2. `LowerCaser` — normalizes to lowercase
/// 3. `RemoveLongFilter(256)` — drops tokens > 256 bytes (protects against pathological input)
///
/// Must be called after `Index::create_in_dir` / `Index::open_in_dir` but before
/// any indexing or searching.
pub fn register_tokenizer(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(256))
        .build();
    index.tokenizers().register(TOKENIZER_NAME, analyzer);
}

// ── Schema versioning ────────────────────────────────────────────────────────

/// Compute a deterministic hash of the current schema definition.
///
/// Changes to field names, types, tokenizer config, or schema version will
/// produce a different hash, triggering a full reindex.
#[must_use]
pub fn schema_hash() -> String {
    let (schema, _) = build_schema();
    let mut entries: Vec<String> = schema
        .fields()
        .map(|(field, entry)| {
            let name = entry.name();
            let field_type = format!("{:?}", entry.field_type());
            format!("{name}:{field_type}:{}", field.field_id())
        })
        .collect();
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(SCHEMA_VERSION.as_bytes());
    hasher.update(b"\n");
    hasher.update(TOKENIZER_NAME.as_bytes());
    hasher.update(b"\n");
    for entry in &entries {
        hasher.update(entry.as_bytes());
        hasher.update(b"\n");
    }
    let result = hasher.finalize();
    hex::encode(result)
}

/// Returns the short schema hash (first 12 hex chars) for directory naming
#[must_use]
pub fn schema_hash_short() -> String {
    let full = schema_hash();
    full[..12.min(full.len())].to_owned()
}

/// Subject field boost factor (applied at query time, not index time)
pub const SUBJECT_BOOST: f32 = 2.0;

/// Body field boost factor (baseline)
pub const BODY_BOOST: f32 = 1.0;

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::TantivyDocument;
    use tantivy::collector::TopDocs;
    use tantivy::doc;
    use tantivy::query::{AllQuery, QueryParser};
    use tantivy::schema::Value;

    #[test]
    fn schema_has_all_fields() {
        let (schema, handles) = build_schema();
        assert_eq!(schema.get_field_name(handles.id), "id");
        assert_eq!(schema.get_field_name(handles.doc_kind), "doc_kind");
        assert_eq!(schema.get_field_name(handles.subject), "subject");
        assert_eq!(schema.get_field_name(handles.body), "body");
        assert_eq!(schema.get_field_name(handles.sender), "sender");
        assert_eq!(schema.get_field_name(handles.project_slug), "project_slug");
        assert_eq!(schema.get_field_name(handles.project_id), "project_id");
        assert_eq!(schema.get_field_name(handles.thread_id), "thread_id");
        assert_eq!(schema.get_field_name(handles.importance), "importance");
        assert_eq!(schema.get_field_name(handles.created_ts), "created_ts");
        assert_eq!(schema.get_field_name(handles.program), "program");
        assert_eq!(schema.get_field_name(handles.model), "model");
    }

    #[test]
    fn schema_field_count() {
        let (schema, _) = build_schema();
        assert_eq!(schema.fields().count(), 12);
    }

    #[test]
    fn schema_hash_deterministic() {
        let h1 = schema_hash();
        let h2 = schema_hash();
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn schema_hash_short_is_12_chars() {
        let short = schema_hash_short();
        assert_eq!(short.len(), 12);
    }

    #[test]
    fn tokenizer_registration_succeeds() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let tokenizer = index.tokenizers().get(TOKENIZER_NAME);
        assert!(tokenizer.is_some());
    }

    #[test]
    fn tokenizer_lowercases_and_splits() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("Hello World!");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn tokenizer_removes_long_tokens() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let long_token = "a".repeat(300);
        let input = format!("short {long_token} word");
        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream(&input);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["short", "word"]);
    }

    #[test]
    fn can_index_and_search_message() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "Migration plan review",
                handles.body => "Here is the plan for DB migration to v3",
                handles.sender => "BlueLake",
                handles.project_slug => "my-project",
                handles.project_id => 1u64,
                handles.thread_id => "br-123",
                handles.importance => "high",
                handles.created_ts => 1_700_000_000_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
        let query = query_parser.parse_query("migration").unwrap();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();

        assert_eq!(top_docs.len(), 1);
        let retrieved: TantivyDocument = searcher.doc(top_docs[0].1).unwrap();
        let id_val = retrieved.get_first(handles.id).unwrap().as_u64().unwrap();
        assert_eq!(id_val, 1);
    }

    #[test]
    fn can_index_and_search_agent() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 7u64,
                handles.doc_kind => "agent",
                handles.subject => "BlueLake",
                handles.body => "BlueLake (claude-code/opus-4.6)\nWorking on search v3",
                handles.project_slug => "my-project",
                handles.project_id => 1u64,
                handles.created_ts => 1_699_000_000_000_000i64,
                handles.program => "claude-code",
                handles.model => "opus-4.6"
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
        let query = query_parser.parse_query("search").unwrap();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();

        assert_eq!(top_docs.len(), 1);
    }

    #[test]
    fn subject_boost_is_higher_than_body() {
        let subject = SUBJECT_BOOST;
        let body = BODY_BOOST;
        assert!(subject > body);
        assert!((subject - 2.0).abs() < f32::EPSILON);
        assert!((body - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn date_field_accepts_micros() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        let ts: i64 = 1_700_000_000_000_000;
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "test",
                handles.body => "test",
                handles.created_ts => ts
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let top_docs = searcher
            .search(&AllQuery, &TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(top_docs.len(), 1);
        let retrieved: TantivyDocument = searcher.doc(top_docs[0].1).unwrap();
        let created = retrieved
            .get_first(handles.created_ts)
            .unwrap()
            .as_i64()
            .unwrap();
        assert_eq!(created, ts);
    }
}
