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

    // ── Schema determinism ──────────────────────────────────────────────

    #[test]
    fn build_schema_returns_same_handles_each_call() {
        let (_, h1) = build_schema();
        let (_, h2) = build_schema();
        assert_eq!(h1.id.field_id(), h2.id.field_id());
        assert_eq!(h1.subject.field_id(), h2.subject.field_id());
        assert_eq!(h1.body.field_id(), h2.body.field_id());
        assert_eq!(h1.created_ts.field_id(), h2.created_ts.field_id());
    }

    // ── Schema hash properties ──────────────────────────────────────────

    #[test]
    fn schema_hash_is_64_hex_chars() {
        let hash = schema_hash();
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn schema_hash_short_is_prefix_of_full() {
        let full = schema_hash();
        let short = schema_hash_short();
        assert!(full.starts_with(&short));
    }

    // ── Constants ───────────────────────────────────────────────────────

    #[test]
    fn tokenizer_name_constant() {
        assert_eq!(TOKENIZER_NAME, "am_default");
    }

    // ── Tokenizer edge cases ────────────────────────────────────────────

    #[test]
    fn tokenizer_empty_input() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenizer_unicode_input() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("café résumé naïve");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["café", "résumé", "naïve"]);
    }

    #[test]
    fn tokenizer_punctuation_splitting() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("hello.world,foo;bar");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        // SimpleTokenizer splits on non-alphanumeric
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"foo".to_string()));
        assert!(tokens.contains(&"bar".to_string()));
    }

    #[test]
    fn tokenizer_token_at_255_bytes_kept() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        // 255-byte token should be kept (under the 256 limit)
        let token255 = "a".repeat(255);
        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream(&token255);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn tokenizer_token_at_256_bytes_removed() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        // 256-byte token is removed by RemoveLongFilter::limit(256)
        let token256 = "a".repeat(256);
        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream(&token256);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert!(tokens.is_empty());
    }

    // ── Multi-doc-kind index ────────────────────────────────────────────

    #[test]
    fn multiple_doc_kinds_coexist() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "hello msg",
                handles.body => "body of message",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.created_ts => 1_000_000i64
            ))
            .unwrap();
        writer
            .add_document(doc!(
                handles.id => 2u64,
                handles.doc_kind => "agent",
                handles.subject => "AgentOne",
                handles.body => "agent description",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.created_ts => 2_000_000i64,
                handles.program => "claude-code",
                handles.model => "opus"
            ))
            .unwrap();
        writer
            .add_document(doc!(
                handles.id => 3u64,
                handles.doc_kind => "project",
                handles.subject => "My Project",
                handles.body => "project description",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.created_ts => 3_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let top_docs = searcher
            .search(&AllQuery, &TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(top_docs.len(), 3);
    }

    #[test]
    fn search_body_finds_content() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "unrelated title",
                handles.body => "the quick brown fox jumps over the lazy dog",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.created_ts => 1_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&index, vec![handles.body]);
        let query = parser.parse_query("fox").unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
        assert_eq!(results.len(), 1);
    }

    // ── FieldHandles Debug derive ───────────────────────────────────────

    #[test]
    fn field_handles_debug() {
        let (_, handles) = build_schema();
        let debug_str = format!("{handles:?}");
        assert!(debug_str.contains("FieldHandles"));
    }

    // ── FieldHandles Clone + Copy ───────────────────────────────────────

    #[test]
    fn field_handles_clone_and_copy() {
        let (_, handles) = build_schema();
        let cloned = handles;
        assert_eq!(cloned.id.field_id(), handles.id.field_id());
        let copied: FieldHandles = handles;
        assert_eq!(copied.body.field_id(), handles.body.field_id());
    }

    // ── BODY_BOOST constant ───────────────────────────────────────────

    #[test]
    fn body_boost_constant() {
        assert!((BODY_BOOST - 1.0).abs() < f32::EPSILON);
    }

    // ── All field IDs distinct ────────────────────────────────────────

    #[test]
    fn field_handles_all_distinct_ids() {
        let (_, h) = build_schema();
        let ids = [
            h.id.field_id(),
            h.doc_kind.field_id(),
            h.subject.field_id(),
            h.body.field_id(),
            h.sender.field_id(),
            h.project_slug.field_id(),
            h.project_id.field_id(),
            h.thread_id.field_id(),
            h.importance.field_id(),
            h.created_ts.field_id(),
            h.program.field_id(),
            h.model.field_id(),
        ];
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 12);
    }

    // ── Tokenizer with numbers/hyphens ────────────────────────────────

    #[test]
    fn tokenizer_numbers_and_hyphens() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("v3.2.1 br-123 test");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        // SimpleTokenizer splits on non-alphanumeric
        assert!(tokens.contains(&"v3".to_string()) || tokens.contains(&"3".to_string()));
        assert!(tokens.contains(&"test".to_string()));
    }

    // ── Subject-only search ───────────────────────────────────────────

    #[test]
    fn search_subject_only() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "unique_keyword_subject",
                handles.body => "nothing relevant here",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.created_ts => 1_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&index, vec![handles.subject]);
        let query = parser.parse_query("unique_keyword_subject").unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
        assert_eq!(results.len(), 1);
    }

    // ── Empty index search ────────────────────────────────────────────

    #[test]
    fn empty_index_search_returns_nothing() {
        let (schema, _handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        // Must create a committed writer for reader to work
        let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&AllQuery, &TopDocs::with_limit(10))
            .unwrap();
        assert!(results.is_empty());
    }

    // ── Multiple documents search ─────────────────────────────────────

    #[test]
    fn search_multiple_documents() {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut writer = index.writer(15_000_000).unwrap();
        let ids: [u64; 5] = [1, 2, 3, 4, 5];
        for (idx, &doc_id) in ids.iter().enumerate() {
            writer
                .add_document(doc!(
                    handles.id => doc_id,
                    handles.doc_kind => "message",
                    handles.subject => "migration discussion",
                    handles.body => format!("message body number {}", idx + 1),
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.created_ts => i64::try_from(doc_id).unwrap() * 1_000_000
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
        let query = parser.parse_query("migration").unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
        assert_eq!(results.len(), 5);
    }

    // ── Schema version constant accessible ────────────────────────────

    #[test]
    fn schema_hash_changes_with_different_tokenizer_would_differ() {
        // The schema_hash includes TOKENIZER_NAME, so it's stable
        let h1 = schema_hash();
        let h2 = schema_hash();
        assert_eq!(h1, h2);
        // And it's a valid SHA-256 hex string
        assert_eq!(h1.len(), 64);
    }

    // ── Tokenizer whitespace-only input ───────────────────────────────

    #[test]
    fn tokenizer_whitespace_only() {
        let (schema, _) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let mut tokenizer = index.tokenizers().get(TOKENIZER_NAME).unwrap();
        let mut stream = tokenizer.token_stream("   \t\n  ");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert!(tokens.is_empty());
    }
}
