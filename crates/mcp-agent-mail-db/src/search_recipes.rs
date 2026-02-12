//! Saved search recipes and query history for the Search Cockpit.
//!
//! Provides persistence for:
//! - **Search recipes** — named, reusable search configurations (query text,
//!   facets, scope, sort) that operators can save, pin, and share via deep links.
//! - **Query history** — automatic log of recent searches for quick recall.
//!
//! Both tables use microsecond `i64` timestamps consistent with the rest of
//! the crate.

use serde::{Deserialize, Serialize};
use sqlmodel::Row;
use sqlmodel_schema::Migration;
use crate::DbConn;

use crate::timestamps::now_micros;

/// Maximum number of saved search recipes returned by [`list_recipes`].
///
/// Prevents unbounded memory growth when many recipes accumulate in the DB.
pub const MAX_RECIPES: usize = 200;

// ─────────────────────────────────────────────────────────────────────
// Scope mode
// ─────────────────────────────────────────────────────────────────────

/// Which scope a search recipe targets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    /// Search within a single project.
    Project,
    /// Search across all projects linked to a product.
    Product,
    /// Search globally (all projects visible to the caller).
    #[default]
    Global,
}

impl ScopeMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Product => "product",
            Self::Global => "global",
        }
    }

    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "project" => Self::Project,
            "product" => Self::Product,
            _ => Self::Global,
        }
    }

    /// Cycle to the next scope mode (for keyboard toggle).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Project => Self::Product,
            Self::Product => Self::Global,
            Self::Global => Self::Project,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Product => "Product",
            Self::Global => "Global",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Search recipe
// ─────────────────────────────────────────────────────────────────────

/// A saved search configuration that can be replayed.
///
/// Stores the complete set of facets, scope, and sort settings so a
/// recipe can be loaded and executed with a single key press.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRecipe {
    pub id: Option<i64>,
    /// Human-readable name (e.g. "Urgent unacked messages").
    pub name: String,
    /// Optional longer description.
    pub description: String,
    /// The query text (may be empty for filter-only recipes).
    pub query_text: String,
    /// Document kind filter: "messages", "agents", "projects", "all".
    pub doc_kind: String,
    /// Scope: "project", "product", "global".
    pub scope_mode: ScopeMode,
    /// When scope is `Project` or `Product`, the corresponding ID.
    pub scope_id: Option<i64>,
    /// Importance filter: empty = any, or comma-separated values.
    pub importance_filter: String,
    /// Ack filter: "any", "required", "`not_required`".
    pub ack_filter: String,
    /// Sort mode: "newest", "oldest", "relevance".
    pub sort_mode: String,
    /// Thread filter (optional).
    pub thread_filter: Option<String>,
    /// Microseconds since epoch.
    pub created_ts: i64,
    /// Microseconds since epoch.
    pub updated_ts: i64,
    /// Whether this recipe is pinned to the top of the list.
    pub pinned: bool,
    /// How many times this recipe has been executed.
    pub use_count: i64,
}

impl Default for SearchRecipe {
    fn default() -> Self {
        let now = now_micros();
        Self {
            id: None,
            name: String::new(),
            description: String::new(),
            query_text: String::new(),
            doc_kind: "messages".to_string(),
            scope_mode: ScopeMode::Global,
            scope_id: None,
            importance_filter: String::new(),
            ack_filter: "any".to_string(),
            sort_mode: "newest".to_string(),
            thread_filter: None,
            created_ts: now,
            updated_ts: now,
            pinned: false,
            use_count: 0,
        }
    }
}

impl SearchRecipe {
    /// Deep-link route string for this recipe.
    #[must_use]
    pub fn route_string(&self) -> String {
        let mut params: Vec<(&str, String)> = Vec::new();

        if !self.query_text.is_empty() {
            params.push(("q", self.query_text.clone()));
        }
        if self.doc_kind != "messages" {
            params.push(("type", self.doc_kind.clone()));
        }
        params.push(("scope", self.scope_mode.as_str().to_string()));
        if let Some(sid) = self.scope_id {
            params.push(("scope_id", sid.to_string()));
        }
        if !self.importance_filter.is_empty() {
            params.push(("imp", self.importance_filter.clone()));
        }
        if self.ack_filter != "any" {
            params.push(("ack", self.ack_filter.clone()));
        }
        if self.sort_mode != "newest" {
            params.push(("sort", self.sort_mode.clone()));
        }
        if let Some(ref tid) = self.thread_filter {
            params.push(("thread", tid.clone()));
        }

        if params.is_empty() {
            return "/search".to_string();
        }

        let mut out = String::from("/search?");
        for (i, (k, v)) in params.iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            out.push_str(k);
            out.push('=');
            out.push_str(v);
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────
// Query history entry
// ─────────────────────────────────────────────────────────────────────

/// A single entry in the query history log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    pub id: Option<i64>,
    /// The raw query text that was executed.
    pub query_text: String,
    /// Document kind searched.
    pub doc_kind: String,
    /// Scope mode at execution time.
    pub scope_mode: ScopeMode,
    /// Scope ID if project/product scoped.
    pub scope_id: Option<i64>,
    /// Number of results returned.
    pub result_count: i64,
    /// When executed (microseconds since epoch).
    pub executed_ts: i64,
}

impl Default for QueryHistoryEntry {
    fn default() -> Self {
        Self {
            id: None,
            query_text: String::new(),
            doc_kind: "messages".to_string(),
            scope_mode: ScopeMode::Global,
            scope_id: None,
            result_count: 0,
            executed_ts: now_micros(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Schema migrations (v8)
// ─────────────────────────────────────────────────────────────────────

/// Returns migrations for search recipe and query history tables.
#[must_use]
pub fn recipe_migrations() -> Vec<Migration> {
    vec![
        Migration::new(
            "v8_create_search_recipes".to_string(),
            "create search_recipes table for saved search configurations".to_string(),
            "CREATE TABLE IF NOT EXISTS search_recipes ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                name TEXT NOT NULL, \
                description TEXT NOT NULL DEFAULT '', \
                query_text TEXT NOT NULL DEFAULT '', \
                doc_kind TEXT NOT NULL DEFAULT 'messages', \
                scope_mode TEXT NOT NULL DEFAULT 'global', \
                scope_id INTEGER, \
                importance_filter TEXT NOT NULL DEFAULT '', \
                ack_filter TEXT NOT NULL DEFAULT 'any', \
                sort_mode TEXT NOT NULL DEFAULT 'newest', \
                thread_filter TEXT, \
                created_ts INTEGER NOT NULL, \
                updated_ts INTEGER NOT NULL, \
                pinned INTEGER NOT NULL DEFAULT 0, \
                use_count INTEGER NOT NULL DEFAULT 0 \
            )"
            .to_string(),
            String::new(),
        ),
        Migration::new(
            "v8_create_query_history".to_string(),
            "create query_history table for recent search log".to_string(),
            "CREATE TABLE IF NOT EXISTS query_history ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                query_text TEXT NOT NULL, \
                doc_kind TEXT NOT NULL DEFAULT 'messages', \
                scope_mode TEXT NOT NULL DEFAULT 'global', \
                scope_id INTEGER, \
                result_count INTEGER NOT NULL DEFAULT 0, \
                executed_ts INTEGER NOT NULL \
            )"
            .to_string(),
            String::new(),
        ),
        Migration::new(
            "v8_idx_query_history_ts".to_string(),
            "index query_history by execution time for recent-first listing".to_string(),
            "CREATE INDEX IF NOT EXISTS idx_query_history_executed_ts \
                ON query_history(executed_ts DESC)"
                .to_string(),
            String::new(),
        ),
        Migration::new(
            "v8_idx_search_recipes_pinned".to_string(),
            "index search_recipes by pinned + name for sorted listing".to_string(),
            "CREATE INDEX IF NOT EXISTS idx_search_recipes_pinned_name \
                ON search_recipes(pinned DESC, name ASC)"
                .to_string(),
            String::new(),
        ),
    ]
}

// ─────────────────────────────────────────────────────────────────────
// Query helpers (sync, for TUI and CLI use)
// ─────────────────────────────────────────────────────────────────────

use crate::sqlmodel::Value;

/// Insert a new search recipe. Returns the row ID.
pub fn insert_recipe(conn: &DbConn, recipe: &SearchRecipe) -> Result<i64, String> {
    let sql = "INSERT INTO search_recipes \
        (name, description, query_text, doc_kind, scope_mode, scope_id, \
         importance_filter, ack_filter, sort_mode, thread_filter, \
         created_ts, updated_ts, pinned, use_count) \
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

    let params = recipe_to_params(recipe);
    conn.execute_sync(sql, &params).map_err(|e| e.to_string())?;

    // Fetch last insert rowid
    let rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| e.to_string())?;
    rows.first()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| "failed to get last_insert_rowid".to_string())
}

/// Update an existing recipe by ID.
pub fn update_recipe(conn: &DbConn, recipe: &SearchRecipe) -> Result<(), String> {
    let Some(id) = recipe.id else {
        return Err("recipe has no ID".to_string());
    };

    let sql = "UPDATE search_recipes SET \
        name = ?, description = ?, query_text = ?, doc_kind = ?, \
        scope_mode = ?, scope_id = ?, importance_filter = ?, \
        ack_filter = ?, sort_mode = ?, thread_filter = ?, \
        updated_ts = ?, pinned = ?, use_count = ? \
        WHERE id = ?";

    let now = now_micros();
    let params = vec![
        Value::Text(recipe.name.clone()),
        Value::Text(recipe.description.clone()),
        Value::Text(recipe.query_text.clone()),
        Value::Text(recipe.doc_kind.clone()),
        Value::Text(recipe.scope_mode.as_str().to_string()),
        recipe.scope_id.map_or(Value::Null, Value::BigInt),
        Value::Text(recipe.importance_filter.clone()),
        Value::Text(recipe.ack_filter.clone()),
        Value::Text(recipe.sort_mode.clone()),
        recipe
            .thread_filter
            .as_ref()
            .map_or(Value::Null, |s| Value::Text(s.clone())),
        Value::BigInt(now),
        Value::BigInt(i64::from(recipe.pinned)),
        Value::BigInt(recipe.use_count),
        Value::BigInt(id),
    ];

    conn.execute_sync(sql, &params).map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete a recipe by ID.
pub fn delete_recipe(conn: &DbConn, recipe_id: i64) -> Result<(), String> {
    conn.execute_sync(
        "DELETE FROM search_recipes WHERE id = ?",
        &[Value::BigInt(recipe_id)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// List recipes, ordered by pinned (desc) then name (asc).
///
/// Returns at most [`MAX_RECIPES`] entries to prevent unbounded memory growth.
pub fn list_recipes(conn: &DbConn) -> Result<Vec<SearchRecipe>, String> {
    let limit_val = i64::try_from(MAX_RECIPES).unwrap_or(200);
    let sql = "SELECT id, name, description, query_text, doc_kind, scope_mode, \
        scope_id, importance_filter, ack_filter, sort_mode, thread_filter, \
        created_ts, updated_ts, pinned, use_count \
        FROM search_recipes \
        ORDER BY pinned DESC, name ASC \
        LIMIT ?";

    let rows = conn
        .query_sync(sql, &[Value::BigInt(limit_val)])
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(row_to_recipe).collect())
}

/// Get a single recipe by ID.
pub fn get_recipe(conn: &DbConn, recipe_id: i64) -> Result<Option<SearchRecipe>, String> {
    let sql = "SELECT id, name, description, query_text, doc_kind, scope_mode, \
        scope_id, importance_filter, ack_filter, sort_mode, thread_filter, \
        created_ts, updated_ts, pinned, use_count \
        FROM search_recipes WHERE id = ?";

    let rows = conn
        .query_sync(sql, &[Value::BigInt(recipe_id)])
        .map_err(|e| e.to_string())?;
    Ok(rows.first().map(row_to_recipe))
}

/// Increment the use count for a recipe and update its timestamp.
pub fn touch_recipe(conn: &DbConn, recipe_id: i64) -> Result<(), String> {
    let now = now_micros();
    conn.execute_sync(
        "UPDATE search_recipes SET use_count = use_count + 1, updated_ts = ? WHERE id = ?",
        &[Value::BigInt(now), Value::BigInt(recipe_id)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Prune old recipes, keeping pinned recipes plus the most recently updated
/// `keep` non-pinned recipes. Returns the number of deleted rows.
pub fn prune_recipes(conn: &DbConn, keep: usize) -> Result<u64, String> {
    let keep_val = i64::try_from(keep.min(10_000)).unwrap_or(200);
    // Keep all pinned recipes unconditionally, plus the `keep` most recently
    // updated non-pinned recipes.
    let sql = "DELETE FROM search_recipes WHERE pinned = 0 AND id NOT IN ( \
        SELECT id FROM search_recipes WHERE pinned = 0 \
        ORDER BY updated_ts DESC LIMIT ? \
    )";
    conn.execute_sync(sql, &[Value::BigInt(keep_val)])
        .map_err(|e| e.to_string())
}

/// Count total recipes.
pub fn count_recipes(conn: &DbConn) -> Result<i64, String> {
    let rows = conn
        .query_sync("SELECT COUNT(*) AS cnt FROM search_recipes", &[])
        .map_err(|e| e.to_string())?;
    rows.first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .ok_or_else(|| "failed to count recipes".to_string())
}

/// Record a query execution in history.
pub fn insert_history(conn: &DbConn, entry: &QueryHistoryEntry) -> Result<i64, String> {
    let sql = "INSERT INTO query_history \
        (query_text, doc_kind, scope_mode, scope_id, result_count, executed_ts) \
        VALUES (?, ?, ?, ?, ?, ?)";

    let params = vec![
        Value::Text(entry.query_text.clone()),
        Value::Text(entry.doc_kind.clone()),
        Value::Text(entry.scope_mode.as_str().to_string()),
        entry.scope_id.map_or(Value::Null, Value::BigInt),
        Value::BigInt(entry.result_count),
        Value::BigInt(entry.executed_ts),
    ];

    conn.execute_sync(sql, &params).map_err(|e| e.to_string())?;

    let rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| e.to_string())?;
    rows.first()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| "failed to get last_insert_rowid".to_string())
}

/// List recent query history, newest first.
pub fn list_recent_history(
    conn: &DbConn,
    limit: usize,
) -> Result<Vec<QueryHistoryEntry>, String> {
    let sql = "SELECT id, query_text, doc_kind, scope_mode, scope_id, \
        result_count, executed_ts \
        FROM query_history \
        ORDER BY executed_ts DESC \
        LIMIT ?";

    let limit_val = i64::try_from(limit.min(500)).unwrap_or(50);
    let rows = conn
        .query_sync(sql, &[Value::BigInt(limit_val)])
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(row_to_history).collect())
}

/// Prune old history entries, keeping only the most recent `keep` entries.
pub fn prune_history(conn: &DbConn, keep: usize) -> Result<u64, String> {
    let keep_val = i64::try_from(keep.min(10_000)).unwrap_or(500);
    let sql = "DELETE FROM query_history WHERE id NOT IN ( \
        SELECT id FROM query_history ORDER BY executed_ts DESC LIMIT ? \
    )";
    conn.execute_sync(sql, &[Value::BigInt(keep_val)])
        .map_err(|e| e.to_string())
}

/// Count total history entries.
pub fn count_history(conn: &DbConn) -> Result<i64, String> {
    let rows = conn
        .query_sync("SELECT COUNT(*) AS cnt FROM query_history", &[])
        .map_err(|e| e.to_string())?;
    rows.first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .ok_or_else(|| "failed to count history".to_string())
}

// ─────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────

fn recipe_to_params(r: &SearchRecipe) -> Vec<Value> {
    vec![
        Value::Text(r.name.clone()),
        Value::Text(r.description.clone()),
        Value::Text(r.query_text.clone()),
        Value::Text(r.doc_kind.clone()),
        Value::Text(r.scope_mode.as_str().to_string()),
        r.scope_id.map_or(Value::Null, Value::BigInt),
        Value::Text(r.importance_filter.clone()),
        Value::Text(r.ack_filter.clone()),
        Value::Text(r.sort_mode.clone()),
        r.thread_filter
            .as_ref()
            .map_or(Value::Null, |s| Value::Text(s.clone())),
        Value::BigInt(r.created_ts),
        Value::BigInt(r.updated_ts),
        Value::BigInt(i64::from(r.pinned)),
        Value::BigInt(r.use_count),
    ]
}

fn row_to_recipe(row: &Row) -> SearchRecipe {
    SearchRecipe {
        id: row.get_named("id").ok(),
        name: row.get_named("name").unwrap_or_default(),
        description: row.get_named("description").unwrap_or_default(),
        query_text: row.get_named("query_text").unwrap_or_default(),
        doc_kind: row
            .get_named("doc_kind")
            .unwrap_or_else(|_| "messages".to_string()),
        scope_mode: row
            .get_named::<String>("scope_mode")
            .map(|s: String| ScopeMode::from_str_lossy(&s))
            .unwrap_or_default(),
        scope_id: row.get_named("scope_id").ok(),
        importance_filter: row.get_named("importance_filter").unwrap_or_default(),
        ack_filter: row
            .get_named("ack_filter")
            .unwrap_or_else(|_| "any".to_string()),
        sort_mode: row
            .get_named("sort_mode")
            .unwrap_or_else(|_| "newest".to_string()),
        thread_filter: row.get_named("thread_filter").ok(),
        created_ts: row.get_named("created_ts").unwrap_or(0),
        updated_ts: row.get_named("updated_ts").unwrap_or(0),
        pinned: row.get_named::<i64>("pinned").is_ok_and(|v| v != 0),
        use_count: row.get_named("use_count").unwrap_or(0),
    }
}

fn row_to_history(row: &Row) -> QueryHistoryEntry {
    QueryHistoryEntry {
        id: row.get_named("id").ok(),
        query_text: row.get_named("query_text").unwrap_or_default(),
        doc_kind: row
            .get_named("doc_kind")
            .unwrap_or_else(|_| "messages".to_string()),
        scope_mode: row
            .get_named::<String>("scope_mode")
            .map(|s: String| ScopeMode::from_str_lossy(&s))
            .unwrap_or_default(),
        scope_id: row.get_named("scope_id").ok(),
        result_count: row.get_named("result_count").unwrap_or(0),
        executed_ts: row.get_named("executed_ts").unwrap_or(0),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> DbConn {
        let conn = DbConn::open_memory().expect("open in-memory db");
        // Run v8 recipe migrations
        for mig in recipe_migrations() {
            conn.execute_sync(&mig.up, &[])
                .unwrap_or_else(|e| panic!("migration {} failed: {e}", mig.id));
        }
        conn
    }

    // ── ScopeMode ─────────────────────────────────────────────────

    #[test]
    fn scope_mode_round_trips() {
        for mode in [ScopeMode::Project, ScopeMode::Product, ScopeMode::Global] {
            assert_eq!(ScopeMode::from_str_lossy(mode.as_str()), mode);
        }
    }

    #[test]
    fn scope_mode_default_is_global() {
        assert_eq!(ScopeMode::default(), ScopeMode::Global);
    }

    #[test]
    fn scope_mode_cycles() {
        assert_eq!(ScopeMode::Project.next(), ScopeMode::Product);
        assert_eq!(ScopeMode::Product.next(), ScopeMode::Global);
        assert_eq!(ScopeMode::Global.next(), ScopeMode::Project);
    }

    #[test]
    fn scope_mode_labels() {
        assert_eq!(ScopeMode::Project.label(), "Project");
        assert_eq!(ScopeMode::Product.label(), "Product");
        assert_eq!(ScopeMode::Global.label(), "Global");
    }

    #[test]
    fn scope_mode_unknown_string_defaults_to_global() {
        assert_eq!(ScopeMode::from_str_lossy("banana"), ScopeMode::Global);
        assert_eq!(ScopeMode::from_str_lossy(""), ScopeMode::Global);
    }

    // ── Recipe CRUD ───────────────────────────────────────────────

    #[test]
    fn insert_and_get_recipe() {
        let conn = open_test_db();
        let recipe = SearchRecipe {
            name: "Urgent unacked".to_string(),
            query_text: "error".to_string(),
            doc_kind: "messages".to_string(),
            importance_filter: "urgent".to_string(),
            ack_filter: "required".to_string(),
            ..Default::default()
        };

        let id = insert_recipe(&conn, &recipe).expect("insert");
        assert!(id > 0);

        let fetched = get_recipe(&conn, id).expect("get").expect("found");
        assert_eq!(fetched.name, "Urgent unacked");
        assert_eq!(fetched.query_text, "error");
        assert_eq!(fetched.importance_filter, "urgent");
        assert_eq!(fetched.ack_filter, "required");
        assert_eq!(fetched.id, Some(id));
    }

    #[test]
    fn update_recipe_persists() {
        let conn = open_test_db();
        let recipe = SearchRecipe {
            name: "v1".to_string(),
            ..Default::default()
        };
        let id = insert_recipe(&conn, &recipe).expect("insert");

        let mut updated = recipe;
        updated.id = Some(id);
        updated.name = "v2".to_string();
        updated.pinned = true;
        update_recipe(&conn, &updated).expect("update");

        let fetched = get_recipe(&conn, id).expect("get").expect("found");
        assert_eq!(fetched.name, "v2");
        assert!(fetched.pinned);
    }

    #[test]
    fn delete_recipe_removes() {
        let conn = open_test_db();
        let recipe = SearchRecipe {
            name: "ephemeral".to_string(),
            ..Default::default()
        };
        let id = insert_recipe(&conn, &recipe).expect("insert");
        delete_recipe(&conn, id).expect("delete");
        assert!(get_recipe(&conn, id).expect("get").is_none());
    }

    #[test]
    fn list_recipes_ordered_by_pinned_then_name() {
        let conn = open_test_db();

        insert_recipe(
            &conn,
            &SearchRecipe {
                name: "Zebra".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        insert_recipe(
            &conn,
            &SearchRecipe {
                name: "Alpha".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        insert_recipe(
            &conn,
            &SearchRecipe {
                name: "Pinned".to_string(),
                pinned: true,
                ..Default::default()
            },
        )
        .unwrap();

        let recipes = list_recipes(&conn).expect("list");
        assert_eq!(recipes.len(), 3);
        assert_eq!(recipes[0].name, "Pinned");
        assert_eq!(recipes[1].name, "Alpha");
        assert_eq!(recipes[2].name, "Zebra");
    }

    #[test]
    fn touch_recipe_increments_use_count() {
        let conn = open_test_db();
        let id = insert_recipe(
            &conn,
            &SearchRecipe {
                name: "counter".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        touch_recipe(&conn, id).unwrap();
        touch_recipe(&conn, id).unwrap();

        let fetched = get_recipe(&conn, id).unwrap().unwrap();
        assert_eq!(fetched.use_count, 2);
    }

    #[test]
    fn recipe_scope_id_nullable() {
        let conn = open_test_db();

        // Without scope_id
        let id1 = insert_recipe(
            &conn,
            &SearchRecipe {
                name: "global".to_string(),
                scope_mode: ScopeMode::Global,
                scope_id: None,
                ..Default::default()
            },
        )
        .unwrap();
        let r1 = get_recipe(&conn, id1).unwrap().unwrap();
        assert!(r1.scope_id.is_none());

        // With scope_id
        let id2 = insert_recipe(
            &conn,
            &SearchRecipe {
                name: "project-scoped".to_string(),
                scope_mode: ScopeMode::Project,
                scope_id: Some(42),
                ..Default::default()
            },
        )
        .unwrap();
        let r2 = get_recipe(&conn, id2).unwrap().unwrap();
        assert_eq!(r2.scope_id, Some(42));
        assert_eq!(r2.scope_mode, ScopeMode::Project);
    }

    // ── Query history ─────────────────────────────────────────────

    #[test]
    fn insert_and_list_history() {
        let conn = open_test_db();
        let ts1 = 1_000_000;
        let ts2 = 2_000_000;

        insert_history(
            &conn,
            &QueryHistoryEntry {
                query_text: "first".to_string(),
                result_count: 10,
                executed_ts: ts1,
                ..Default::default()
            },
        )
        .unwrap();

        insert_history(
            &conn,
            &QueryHistoryEntry {
                query_text: "second".to_string(),
                result_count: 5,
                executed_ts: ts2,
                ..Default::default()
            },
        )
        .unwrap();

        let history = list_recent_history(&conn, 10).expect("list");
        assert_eq!(history.len(), 2);
        // Newest first
        assert_eq!(history[0].query_text, "second");
        assert_eq!(history[0].result_count, 5);
        assert_eq!(history[1].query_text, "first");
    }

    #[test]
    fn history_limit_respected() {
        let conn = open_test_db();
        for i in 0..20 {
            insert_history(
                &conn,
                &QueryHistoryEntry {
                    query_text: format!("q{i}"),
                    executed_ts: i64::from(i) * 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let history = list_recent_history(&conn, 5).expect("list");
        assert_eq!(history.len(), 5);
        // Newest first
        assert_eq!(history[0].query_text, "q19");
    }

    #[test]
    fn prune_history_keeps_recent() {
        let conn = open_test_db();
        for i in 0..10 {
            insert_history(
                &conn,
                &QueryHistoryEntry {
                    query_text: format!("q{i}"),
                    executed_ts: i64::from(i) * 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let deleted = prune_history(&conn, 3).expect("prune");
        assert_eq!(deleted, 7);

        let remaining = list_recent_history(&conn, 100).expect("list");
        assert_eq!(remaining.len(), 3);
        // Only the 3 newest should survive
        assert_eq!(remaining[0].query_text, "q9");
        assert_eq!(remaining[1].query_text, "q8");
        assert_eq!(remaining[2].query_text, "q7");
    }

    #[test]
    fn count_history_works() {
        let conn = open_test_db();
        assert_eq!(count_history(&conn).unwrap(), 0);

        insert_history(
            &conn,
            &QueryHistoryEntry {
                query_text: "test".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_history(&conn).unwrap(), 1);
    }

    #[test]
    fn history_scope_persists() {
        let conn = open_test_db();
        insert_history(
            &conn,
            &QueryHistoryEntry {
                query_text: "scoped".to_string(),
                scope_mode: ScopeMode::Product,
                scope_id: Some(99),
                ..Default::default()
            },
        )
        .unwrap();

        let history = list_recent_history(&conn, 1).unwrap();
        assert_eq!(history[0].scope_mode, ScopeMode::Product);
        assert_eq!(history[0].scope_id, Some(99));
    }

    // ── Recipe route string ───────────────────────────────────────

    #[test]
    fn recipe_route_string_minimal() {
        let recipe = SearchRecipe::default();
        let route = recipe.route_string();
        assert!(route.contains("/search"));
        assert!(route.contains("scope=global"));
    }

    #[test]
    fn recipe_route_string_full() {
        let recipe = SearchRecipe {
            query_text: "error".to_string(),
            doc_kind: "all".to_string(),
            scope_mode: ScopeMode::Project,
            scope_id: Some(1),
            importance_filter: "urgent,high".to_string(),
            ack_filter: "required".to_string(),
            sort_mode: "relevance".to_string(),
            thread_filter: Some("t-1".to_string()),
            ..Default::default()
        };
        let route = recipe.route_string();
        assert!(route.contains("q=error"));
        assert!(route.contains("type=all"));
        assert!(route.contains("scope=project"));
        assert!(route.contains("scope_id=1"));
        assert!(route.contains("imp=urgent,high"));
        assert!(route.contains("ack=required"));
        assert!(route.contains("sort=relevance"));
        assert!(route.contains("thread=t-1"));
    }

    // ── Serialization ─────────────────────────────────────────────

    #[test]
    fn scope_mode_serde_round_trip() {
        let json = serde_json::to_string(&ScopeMode::Product).unwrap();
        assert_eq!(json, "\"product\"");
        let back: ScopeMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ScopeMode::Product);
    }

    #[test]
    fn recipe_serde_round_trip() {
        let recipe = SearchRecipe {
            name: "test".to_string(),
            scope_mode: ScopeMode::Project,
            scope_id: Some(5),
            pinned: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&recipe).unwrap();
        let back: SearchRecipe = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test");
        assert_eq!(back.scope_mode, ScopeMode::Project);
        assert_eq!(back.scope_id, Some(5));
        assert!(back.pinned);
    }

    // ── Migration idempotency ─────────────────────────────────────

    #[test]
    fn migrations_are_idempotent() {
        let conn = DbConn::open_memory().expect("open");
        // Run twice — should not error
        for _ in 0..2 {
            for mig in recipe_migrations() {
                conn.execute_sync(&mig.up, &[])
                    .unwrap_or_else(|e| panic!("migration {} failed on re-run: {e}", mig.id));
            }
        }
    }

    // ── Update without ID errors ──────────────────────────────────

    #[test]
    fn update_recipe_without_id_errors() {
        let conn = open_test_db();
        let recipe = SearchRecipe::default(); // id = None
        let result = update_recipe(&conn, &recipe);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no ID"));
    }

    // ── Capacity / pruning ───────────────────────────────────────

    #[test]
    fn list_recipes_respects_limit() {
        let conn = open_test_db();
        // Insert MAX_RECIPES + 10 recipes.
        for i in 0..(MAX_RECIPES + 10) {
            insert_recipe(
                &conn,
                &SearchRecipe {
                    name: format!("r{i:04}"),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let recipes = list_recipes(&conn).expect("list");
        assert_eq!(recipes.len(), MAX_RECIPES);
    }

    #[test]
    fn count_recipes_works() {
        let conn = open_test_db();
        assert_eq!(count_recipes(&conn).unwrap(), 0);

        insert_recipe(
            &conn,
            &SearchRecipe {
                name: "one".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_recipes(&conn).unwrap(), 1);
    }

    #[test]
    fn prune_recipes_keeps_pinned_and_recent() {
        let conn = open_test_db();

        // Insert 5 non-pinned recipes with staggered timestamps.
        for i in 0..5 {
            let mut r = SearchRecipe {
                name: format!("np{i}"),
                ..Default::default()
            };
            r.updated_ts = i64::from(i) * 1_000_000;
            insert_recipe(&conn, &r).unwrap();
        }

        // Insert 2 pinned recipes.
        for i in 0..2 {
            insert_recipe(
                &conn,
                &SearchRecipe {
                    name: format!("pinned{i}"),
                    pinned: true,
                    updated_ts: 100_000, // old timestamp
                    ..Default::default()
                },
            )
            .unwrap();
        }

        // Prune keeping only the 2 most recent non-pinned.
        let deleted = prune_recipes(&conn, 2).expect("prune");
        assert_eq!(deleted, 3); // 5 - 2 = 3 old non-pinned

        let remaining = list_recipes(&conn).expect("list");
        // 2 pinned + 2 non-pinned = 4
        assert_eq!(remaining.len(), 4);
        // Pinned recipes must survive regardless of age.
        let pinned_count = remaining.iter().filter(|r| r.pinned).count();
        assert_eq!(pinned_count, 2);
        // The surviving non-pinned should be the newest (np3, np4).
        let non_pinned: Vec<_> = remaining.iter().filter(|r| !r.pinned).collect();
        assert_eq!(non_pinned.len(), 2);
        assert!(non_pinned.iter().any(|r| r.name == "np3"));
        assert!(non_pinned.iter().any(|r| r.name == "np4"));
    }

    #[test]
    fn prune_recipes_noop_when_within_limit() {
        let conn = open_test_db();
        for i in 0..3 {
            insert_recipe(
                &conn,
                &SearchRecipe {
                    name: format!("r{i}"),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let deleted = prune_recipes(&conn, 10).expect("prune");
        assert_eq!(deleted, 0);
        assert_eq!(count_recipes(&conn).unwrap(), 3);
    }
}
