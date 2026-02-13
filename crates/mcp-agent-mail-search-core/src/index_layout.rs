//! Index directory layout, schema hashing, and checkpoints
//!
//! Manages on-disk structure for search indexes:
//! - Per-project and global index directories
//! - Schema versioning via content hashing
//! - Checkpoint metadata for atomic rebuilds
//! - Rollback-safe index activation via directory swaps

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::error::{SearchError, SearchResult};

/// Schema version derived from a content hash of the index field definitions.
///
/// When the schema changes, the hash changes, triggering a full rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchemaHash(pub String);

impl SchemaHash {
    /// Compute a schema hash from a list of field definitions.
    ///
    /// Fields are sorted for deterministic hashing regardless of declaration order.
    #[must_use]
    pub fn compute(fields: &[SchemaField]) -> Self {
        let mut sorted: Vec<String> = fields
            .iter()
            .map(|f| format!("{}:{}:{}", f.name, f.field_type, f.indexed))
            .collect();
        sorted.sort();

        let mut hasher = Sha256::new();
        for entry in &sorted {
            hasher.update(entry.as_bytes());
            hasher.update(b"\n");
        }
        let result = hasher.finalize();
        Self(hex::encode(result))
    }

    /// Returns the short hash (first 12 hex chars) for directory naming
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..12.min(self.0.len())]
    }
}

/// A field definition used for schema hashing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaField {
    /// Field name (e.g., "body", "title", "sender")
    pub name: String,
    /// Field type descriptor (e.g., "text", "i64", "bytes")
    pub field_type: String,
    /// Whether the field is indexed (vs stored-only)
    pub indexed: bool,
}

/// Checkpoint metadata for an index build or rebuild.
///
/// Written atomically alongside the index data to track build state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCheckpoint {
    /// Schema hash at the time of this build
    pub schema_hash: SchemaHash,
    /// Total documents indexed in this build
    pub docs_indexed: usize,
    /// Timestamp when the build started (micros since epoch)
    pub started_ts: i64,
    /// Timestamp when the build completed (micros since epoch, None if in-progress)
    pub completed_ts: Option<i64>,
    /// The highest document version included in this build
    pub max_version: i64,
    /// Whether this build completed successfully
    pub success: bool,
}

impl IndexCheckpoint {
    /// The filename for checkpoint metadata
    pub const FILENAME: &'static str = "checkpoint.json";

    /// Write checkpoint to a file
    ///
    /// # Errors
    /// Returns `SearchError::Io` on write failure.
    pub fn write_to(&self, dir: &Path) -> SearchResult<()> {
        let path = dir.join(Self::FILENAME);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Read checkpoint from a directory
    ///
    /// # Errors
    /// Returns `SearchError::Io` if the file doesn't exist or can't be read.
    pub fn read_from(dir: &Path) -> SearchResult<Self> {
        let path = dir.join(Self::FILENAME);
        let json = std::fs::read_to_string(path)?;
        let checkpoint: Self = serde_json::from_str(&json)?;
        Ok(checkpoint)
    }
}

/// Scope for an index — determines the directory path
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndexScope {
    /// Per-project index
    Project { project_id: i64 },
    /// Cross-project product index
    Product { product_id: i64 },
    /// Global index across all projects
    Global,
}

impl IndexScope {
    /// Returns the directory name component for this scope
    #[must_use]
    pub fn dir_name(&self) -> String {
        match self {
            Self::Project { project_id } => format!("project-{project_id}"),
            Self::Product { product_id } => format!("product-{product_id}"),
            Self::Global => "global".to_owned(),
        }
    }
}

/// Manages the on-disk layout for search indexes.
///
/// Directory structure:
/// ```text
/// {root}/
///   indexes/
///     {scope}/                    # e.g., "project-1", "global"
///       lexical/
///         {schema_hash_short}/    # Tantivy index files
///           checkpoint.json
///           ...tantivy files...
///       semantic/
///         {schema_hash_short}/    # Vector index files
///           checkpoint.json
///           ...vector files...
///       active -> lexical/{hash}  # Symlink to active index version
/// ```
#[derive(Debug, Clone)]
pub struct IndexLayout {
    /// Root directory for all indexes
    root: PathBuf,
}

impl IndexLayout {
    /// Create a new index layout rooted at the given directory
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the root directory
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the base directory for a given scope
    #[must_use]
    pub fn scope_dir(&self, scope: &IndexScope) -> PathBuf {
        self.root.join("indexes").join(scope.dir_name())
    }

    /// Returns the directory for a lexical index with a specific schema hash
    #[must_use]
    pub fn lexical_dir(&self, scope: &IndexScope, schema: &SchemaHash) -> PathBuf {
        self.scope_dir(scope).join("lexical").join(schema.short())
    }

    /// Returns the directory for a semantic index with a specific schema hash
    #[must_use]
    pub fn semantic_dir(&self, scope: &IndexScope, schema: &SchemaHash) -> PathBuf {
        self.scope_dir(scope).join("semantic").join(schema.short())
    }

    /// Returns the path to the "active" symlink for a given scope and engine
    #[must_use]
    pub fn active_link(&self, scope: &IndexScope, engine: &str) -> PathBuf {
        self.scope_dir(scope).join(format!("active-{engine}"))
    }

    /// Ensure all directories exist for a given scope and schema.
    ///
    /// # Errors
    /// Returns `SearchError::Io` if directory creation fails.
    pub fn ensure_dirs(&self, scope: &IndexScope, schema: &SchemaHash) -> SearchResult<()> {
        std::fs::create_dir_all(self.lexical_dir(scope, schema))?;
        std::fs::create_dir_all(self.semantic_dir(scope, schema))?;
        Ok(())
    }

    /// Atomically activate a new index version by updating the active symlink.
    ///
    /// Uses a rename-based approach for atomic swaps on Unix.
    ///
    /// # Errors
    /// Returns `SearchError::Io` on filesystem errors.
    pub fn activate(
        &self,
        scope: &IndexScope,
        engine: &str,
        schema: &SchemaHash,
    ) -> SearchResult<()> {
        let link_path = self.active_link(scope, engine);
        let target = match engine {
            "lexical" => self.lexical_dir(scope, schema),
            "semantic" => self.semantic_dir(scope, schema),
            other => {
                return Err(SearchError::InvalidQuery(format!(
                    "Unknown engine type: {other}"
                )));
            }
        };

        // Create a temporary symlink and atomically rename it
        let tmp_link = link_path.with_extension("tmp");
        // Remove stale tmp link if it exists
        let _ = std::fs::remove_file(&tmp_link);

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &tmp_link)?;

        #[cfg(not(unix))]
        {
            // Fallback for non-Unix: just write the path as a file
            std::fs::write(&tmp_link, target.to_string_lossy().as_bytes())?;
        }

        std::fs::rename(&tmp_link, &link_path)?;
        Ok(())
    }

    /// Check which schema hash is currently active for a scope and engine.
    ///
    /// Returns `None` if no active index exists.
    #[must_use]
    pub fn active_schema(&self, scope: &IndexScope, engine: &str) -> Option<String> {
        let link_path = self.active_link(scope, engine);
        std::fs::read_link(&link_path)
            .ok()
            .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    /// Check if a schema hash is compatible with the currently active index.
    ///
    /// Returns `true` if:
    /// - No active index exists (first build)
    /// - The active schema matches the given hash
    #[must_use]
    pub fn is_schema_compatible(
        &self,
        scope: &IndexScope,
        engine: &str,
        schema: &SchemaHash,
    ) -> bool {
        self.active_schema(scope, engine)
            .is_none_or(|active_short| active_short == schema.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fields() -> Vec<SchemaField> {
        vec![
            SchemaField {
                name: "body".to_owned(),
                field_type: "text".to_owned(),
                indexed: true,
            },
            SchemaField {
                name: "title".to_owned(),
                field_type: "text".to_owned(),
                indexed: true,
            },
            SchemaField {
                name: "sender".to_owned(),
                field_type: "text".to_owned(),
                indexed: true,
            },
            SchemaField {
                name: "created_ts".to_owned(),
                field_type: "i64".to_owned(),
                indexed: true,
            },
        ]
    }

    #[test]
    fn schema_hash_deterministic() {
        let fields = sample_fields();
        let h1 = SchemaHash::compute(&fields);
        let h2 = SchemaHash::compute(&fields);
        assert_eq!(h1, h2);
    }

    #[test]
    fn schema_hash_order_independent() {
        let mut fields1 = sample_fields();
        let mut fields2 = sample_fields();
        fields2.reverse();
        let h1 = SchemaHash::compute(&fields1);
        let h2 = SchemaHash::compute(&fields2);
        assert_eq!(h1, h2, "Hash should be independent of field order");

        // But changing a field should change the hash
        fields1[0].name = "content".to_owned();
        let h3 = SchemaHash::compute(&fields1);
        assert_ne!(h1, h3, "Different fields should produce different hashes");
    }

    #[test]
    fn schema_hash_short() {
        let fields = sample_fields();
        let hash = SchemaHash::compute(&fields);
        assert_eq!(hash.short().len(), 12);
        assert!(hash.0.len() >= 12);
    }

    #[test]
    fn index_scope_dir_name() {
        assert_eq!(
            IndexScope::Project { project_id: 42 }.dir_name(),
            "project-42"
        );
        assert_eq!(
            IndexScope::Product { product_id: 7 }.dir_name(),
            "product-7"
        );
        assert_eq!(IndexScope::Global.dir_name(), "global");
    }

    #[test]
    fn layout_paths() {
        let layout = IndexLayout::new("/tmp/search");
        let scope = IndexScope::Project { project_id: 1 };
        let schema = SchemaHash("abcdef123456789".to_owned());

        assert_eq!(
            layout.scope_dir(&scope),
            PathBuf::from("/tmp/search/indexes/project-1")
        );
        assert_eq!(
            layout.lexical_dir(&scope, &schema),
            PathBuf::from("/tmp/search/indexes/project-1/lexical/abcdef123456")
        );
        assert_eq!(
            layout.semantic_dir(&scope, &schema),
            PathBuf::from("/tmp/search/indexes/project-1/semantic/abcdef123456")
        );
        assert_eq!(
            layout.active_link(&scope, "lexical"),
            PathBuf::from("/tmp/search/indexes/project-1/active-lexical")
        );
    }

    #[test]
    fn ensure_dirs_creates_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash::compute(&sample_fields());

        layout.ensure_dirs(&scope, &schema).unwrap();

        assert!(layout.lexical_dir(&scope, &schema).exists());
        assert!(layout.semantic_dir(&scope, &schema).exists());
    }

    #[test]
    fn checkpoint_write_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let checkpoint = IndexCheckpoint {
            schema_hash: SchemaHash("abc123".to_owned()),
            docs_indexed: 500,
            started_ts: 1_700_000_000_000_000,
            completed_ts: Some(1_700_000_001_000_000),
            max_version: 1_700_000_000_500_000,
            success: true,
        };

        checkpoint.write_to(tmp.path()).unwrap();
        let loaded = IndexCheckpoint::read_from(tmp.path()).unwrap();

        assert_eq!(loaded.schema_hash, checkpoint.schema_hash);
        assert_eq!(loaded.docs_indexed, 500);
        assert!(loaded.success);
        assert_eq!(loaded.completed_ts, Some(1_700_000_001_000_000));
    }

    #[test]
    fn checkpoint_read_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = IndexCheckpoint::read_from(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn activate_and_check_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Project { project_id: 1 };
        let schema = SchemaHash::compute(&sample_fields());

        // Ensure dirs first
        layout.ensure_dirs(&scope, &schema).unwrap();

        // Before activation, no active schema
        assert!(layout.active_schema(&scope, "lexical").is_none());
        assert!(layout.is_schema_compatible(&scope, "lexical", &schema));

        // Activate
        layout.activate(&scope, "lexical", &schema).unwrap();

        // After activation, schema should match
        let active = layout.active_schema(&scope, "lexical");
        assert!(active.is_some());
        assert_eq!(active.unwrap(), schema.short());
        assert!(layout.is_schema_compatible(&scope, "lexical", &schema));

        // Different schema should be incompatible
        let other_schema = SchemaHash("different12345".to_owned());
        assert!(!layout.is_schema_compatible(&scope, "lexical", &other_schema));
    }

    #[test]
    fn activate_invalid_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123".to_owned());

        let result = layout.activate(&scope, "invalid", &schema);
        assert!(result.is_err());
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let checkpoint = IndexCheckpoint {
            schema_hash: SchemaHash("deadbeef".to_owned()),
            docs_indexed: 1000,
            started_ts: 1_700_000_000_000_000,
            completed_ts: None,
            max_version: 0,
            success: false,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let loaded: IndexCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.docs_indexed, 1000);
        assert!(!loaded.success);
        assert!(loaded.completed_ts.is_none());
    }

    #[test]
    fn schema_field_serde() {
        let field = SchemaField {
            name: "body".to_owned(),
            field_type: "text".to_owned(),
            indexed: true,
        };
        let json = serde_json::to_string(&field).unwrap();
        let field2: SchemaField = serde_json::from_str(&json).unwrap();
        assert_eq!(field2.name, "body");
        assert!(field2.indexed);
    }

    // ── SchemaHash serde + traits ───────────────────────────────────────

    #[test]
    fn schema_hash_serde_roundtrip() {
        let hash = SchemaHash::compute(&sample_fields());
        let json = serde_json::to_string(&hash).unwrap();
        let back: SchemaHash = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hash);
    }

    #[test]
    fn schema_hash_in_hashset() {
        use std::collections::HashSet;
        let h1 = SchemaHash::compute(&sample_fields());
        let h2 = SchemaHash("different".to_owned());
        let mut set = HashSet::new();
        set.insert(h1.clone());
        set.insert(h2);
        set.insert(h1); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn schema_hash_empty_fields() {
        let hash = SchemaHash::compute(&[]);
        assert_eq!(hash.0.len(), 64); // SHA-256 = 64 hex chars
        assert_eq!(hash.short().len(), 12);
    }

    #[test]
    fn schema_hash_is_hex() {
        let hash = SchemaHash::compute(&sample_fields());
        assert!(hash.0.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn schema_hash_debug() {
        let hash = SchemaHash::compute(&sample_fields());
        let debug = format!("{hash:?}");
        assert!(debug.contains("SchemaHash"));
    }

    // ── IndexScope serde ────────────────────────────────────────────────

    #[test]
    fn index_scope_serde_all_variants() {
        let scopes = vec![
            IndexScope::Project { project_id: 42 },
            IndexScope::Product { product_id: 7 },
            IndexScope::Global,
        ];
        for scope in &scopes {
            let json = serde_json::to_string(scope).unwrap();
            let back: IndexScope = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, scope);
        }
    }

    #[test]
    fn index_scope_hash_distinct() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(IndexScope::Project { project_id: 1 });
        set.insert(IndexScope::Product { product_id: 1 });
        set.insert(IndexScope::Global);
        assert_eq!(set.len(), 3);
    }

    // ── IndexLayout ─────────────────────────────────────────────────────

    #[test]
    fn layout_root_accessor() {
        let layout = IndexLayout::new("/data/search");
        assert_eq!(layout.root(), Path::new("/data/search"));
    }

    #[test]
    fn layout_global_scope_paths() {
        let layout = IndexLayout::new("/tmp/idx");
        let scope = IndexScope::Global;
        let schema = SchemaHash("abcdef123456xxxx".to_owned());
        assert_eq!(
            layout.scope_dir(&scope),
            PathBuf::from("/tmp/idx/indexes/global")
        );
        assert_eq!(
            layout.lexical_dir(&scope, &schema),
            PathBuf::from("/tmp/idx/indexes/global/lexical/abcdef123456")
        );
    }

    #[test]
    fn layout_product_scope_paths() {
        let layout = IndexLayout::new("/tmp/idx");
        let scope = IndexScope::Product { product_id: 99 };
        assert_eq!(
            layout.scope_dir(&scope),
            PathBuf::from("/tmp/idx/indexes/product-99")
        );
    }

    #[test]
    fn layout_debug_and_clone() {
        let layout = IndexLayout::new("/tmp");
        let debug = format!("{layout:?}");
        assert!(debug.contains("IndexLayout"));
        let cloned = layout.clone();
        assert_eq!(cloned.root(), layout.root());
    }

    // ── IndexCheckpoint ─────────────────────────────────────────────────

    #[test]
    fn checkpoint_filename_constant() {
        assert_eq!(IndexCheckpoint::FILENAME, "checkpoint.json");
    }

    #[test]
    fn checkpoint_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let cp1 = IndexCheckpoint {
            schema_hash: SchemaHash("v1".to_owned()),
            docs_indexed: 100,
            started_ts: 1_000_000,
            completed_ts: Some(2_000_000),
            max_version: 500_000,
            success: true,
        };
        cp1.write_to(tmp.path()).unwrap();

        let cp2 = IndexCheckpoint {
            schema_hash: SchemaHash("v2".to_owned()),
            docs_indexed: 200,
            started_ts: 3_000_000,
            completed_ts: None,
            max_version: 1_500_000,
            success: false,
        };
        cp2.write_to(tmp.path()).unwrap();

        let loaded = IndexCheckpoint::read_from(tmp.path()).unwrap();
        assert_eq!(loaded.schema_hash, SchemaHash("v2".to_owned()));
        assert_eq!(loaded.docs_indexed, 200);
        assert!(!loaded.success);
    }

    #[test]
    fn checkpoint_file_is_pretty_json() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = IndexCheckpoint {
            schema_hash: SchemaHash("test".to_owned()),
            docs_indexed: 1,
            started_ts: 100,
            completed_ts: None,
            max_version: 0,
            success: true,
        };
        cp.write_to(tmp.path()).unwrap();
        let content = std::fs::read_to_string(tmp.path().join(IndexCheckpoint::FILENAME)).unwrap();
        // Pretty-printed JSON has newlines
        assert!(content.contains('\n'));
    }

    // ── activate semantic engine ────────────────────────────────────────

    #[test]
    fn activate_semantic_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash::compute(&sample_fields());

        layout.ensure_dirs(&scope, &schema).unwrap();
        layout.activate(&scope, "semantic", &schema).unwrap();

        let active = layout.active_schema(&scope, "semantic");
        assert!(active.is_some());
        assert_eq!(active.unwrap(), schema.short());
    }

    // ── SchemaField not-indexed ─────────────────────────────────────────

    #[test]
    fn schema_field_not_indexed() {
        let field = SchemaField {
            name: "metadata".to_owned(),
            field_type: "bytes".to_owned(),
            indexed: false,
        };
        let json = serde_json::to_string(&field).unwrap();
        let back: SchemaField = serde_json::from_str(&json).unwrap();
        assert!(!back.indexed);
        assert_eq!(back.field_type, "bytes");
    }

    // ── SchemaHash short on very short hash ─────────────────────────────

    #[test]
    fn schema_hash_short_on_short_input() {
        let hash = SchemaHash("abc".to_owned());
        // short() should return "abc" (min of 12 and 3 = 3)
        assert_eq!(hash.short(), "abc");
    }
}
