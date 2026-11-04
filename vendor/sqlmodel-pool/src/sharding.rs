//! Horizontal sharding support for SQLModel Rust.
//!
//! This module provides infrastructure for partitioning data across multiple
//! database shards based on a shard key.
//!
//! # Overview
//!
//! Horizontal sharding distributes rows across multiple databases based on a
//! shard key (e.g., `user_id`, `tenant_id`). This enables:
//!
//! - Horizontal scalability beyond single-database limits
//! - Data isolation between tenants/regions
//! - Improved query performance through data locality
//!
//! # Example
//!
//! ```rust,ignore
//! use sqlmodel_pool::{Pool, PoolConfig, ShardedPool, ShardChooser};
//! use sqlmodel_core::{Model, Value};
//!
//! // Define a shard chooser based on modulo hashing
//! struct ModuloShardChooser {
//!     shard_count: usize,
//! }
//!
//! impl ShardChooser for ModuloShardChooser {
//!     fn choose_for_model(&self, shard_key: &Value) -> String {
//!         let id = match shard_key {
//!             Value::BigInt(n) => *n as usize,
//!             Value::Int(n) => *n as usize,
//!             _ => 0,
//!         };
//!         format!("shard_{}", id % self.shard_count)
//!     }
//!
//!     fn choose_for_query(&self, _hints: &QueryHints) -> Vec<String> {
//!         // Query all shards by default
//!         (0..self.shard_count)
//!             .map(|i| format!("shard_{}", i))
//!             .collect()
//!     }
//! }
//!
//! // Create sharded pool
//! let mut sharded_pool = ShardedPool::new(ModuloShardChooser { shard_count: 3 });
//! sharded_pool.add_shard("shard_0", pool_0);
//! sharded_pool.add_shard("shard_1", pool_1);
//! sharded_pool.add_shard("shard_2", pool_2);
//!
//! // Insert routes to correct shard based on model's shard key
//! let order = Order { user_id: 42, ... };
//! let shard = sharded_pool.choose_for_model(&order);
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use asupersync::{Cx, Outcome};
use sqlmodel_core::error::{PoolError, PoolErrorKind};
use sqlmodel_core::{Connection, Error, Model, Value};

use crate::{Pool, PoolConfig, PooledConnection};

/// Hints for query routing when a specific shard key isn't available.
///
/// When executing queries that don't have a clear shard key (e.g., range queries,
/// aggregations), these hints help the `ShardChooser` decide which shards to query.
#[derive(Debug, Clone, Default)]
pub struct QueryHints {
    /// Specific shard names to target (if known).
    pub target_shards: Option<Vec<String>>,

    /// Whether to query all shards (scatter-gather).
    pub scatter_gather: bool,

    /// Optional shard key value extracted from query predicates.
    pub shard_key_value: Option<Value>,

    /// Query type hint (e.g., "select", "aggregate", "count").
    pub query_type: Option<String>,
}

impl QueryHints {
    /// Create empty hints (defaults to scatter-gather).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Target specific shards by name.
    #[must_use]
    pub fn target(mut self, shards: Vec<String>) -> Self {
        self.target_shards = Some(shards);
        self
    }

    /// Enable scatter-gather mode (query all shards).
    #[must_use]
    pub fn scatter_gather(mut self) -> Self {
        self.scatter_gather = true;
        self
    }

    /// Provide a shard key value for routing.
    #[must_use]
    pub fn with_shard_key(mut self, value: Value) -> Self {
        self.shard_key_value = Some(value);
        self
    }

    /// Set the query type hint.
    #[must_use]
    pub fn query_type(mut self, query_type: impl Into<String>) -> Self {
        self.query_type = Some(query_type.into());
        self
    }
}

/// Trait for determining which shard(s) to use for operations.
///
/// Implement this trait to define your sharding strategy. Common strategies:
///
/// - **Modulo hashing**: `shard_key % shard_count`
/// - **Range-based**: Partition by key ranges (e.g., user IDs 0-1M → shard_0)
/// - **Consistent hashing**: Minimize rebalancing when adding/removing shards
/// - **Tenant-based**: Map tenant IDs directly to shard names
///
/// # Example
///
/// ```rust,ignore
/// struct TenantShardChooser {
///     tenant_to_shard: HashMap<String, String>,
///     default_shard: String,
/// }
///
/// impl ShardChooser for TenantShardChooser {
///     fn choose_for_model(&self, shard_key: &Value) -> String {
///         if let Value::Text(tenant_id) = shard_key {
///             self.tenant_to_shard
///                 .get(tenant_id)
///                 .cloned()
///                 .unwrap_or_else(|| self.default_shard.clone())
///         } else {
///             self.default_shard.clone()
///         }
///     }
///
///     fn choose_for_query(&self, hints: &QueryHints) -> Vec<String> {
///         if let Some(Value::Text(tenant_id)) = &hints.shard_key_value {
///             vec![self.choose_for_model(&Value::Text(tenant_id.clone()))]
///         } else {
///             // Query all shards
///             self.tenant_to_shard.values().cloned().collect()
///         }
///     }
/// }
/// ```
pub trait ShardChooser: Send + Sync {
    /// Choose the shard for a model based on its shard key value.
    ///
    /// This is used for INSERT, UPDATE, and DELETE operations where the
    /// shard key is known from the model instance.
    ///
    /// # Arguments
    ///
    /// * `shard_key` - The value of the model's shard key field
    ///
    /// # Returns
    ///
    /// The name of the shard to use (must match a shard registered in `ShardedPool`).
    fn choose_for_model(&self, shard_key: &Value) -> String;

    /// Choose which shards to query based on query hints.
    ///
    /// For queries where the shard key isn't directly available (e.g., range
    /// queries, joins, aggregations), this method returns the list of shards
    /// to query.
    ///
    /// # Arguments
    ///
    /// * `hints` - Query routing hints (target shards, shard key value, etc.)
    ///
    /// # Returns
    ///
    /// List of shard names to query. For point queries with a known shard key,
    /// this should return a single shard. For scatter-gather, return all shards.
    fn choose_for_query(&self, hints: &QueryHints) -> Vec<String>;

    /// Get all registered shard names.
    ///
    /// Default implementation returns an empty vec; override if your chooser
    /// tracks shard names internally.
    fn all_shards(&self) -> Vec<String> {
        vec![]
    }
}

/// A simple modulo-based shard chooser for numeric shard keys.
///
/// Routes based on `shard_key % shard_count`, producing shard names like
/// `shard_0`, `shard_1`, etc.
///
/// This is suitable for evenly distributed numeric keys (e.g., auto-increment IDs).
/// Not suitable for sequential inserts (hotspotting on latest shard) or
/// non-numeric keys.
#[derive(Debug, Clone)]
pub struct ModuloShardChooser {
    shard_count: usize,
    shard_prefix: String,
}

impl ModuloShardChooser {
    /// Create a new modulo shard chooser with the given number of shards.
    ///
    /// Shards are named `shard_0`, `shard_1`, ..., `shard_{n-1}`.
    ///
    /// # Panics
    ///
    /// Panics if `shard_count` is 0, as this would cause division by zero
    /// when routing to shards.
    #[must_use]
    pub fn new(shard_count: usize) -> Self {
        assert!(shard_count > 0, "shard_count must be greater than 0");
        Self {
            shard_count,
            shard_prefix: "shard_".to_string(),
        }
    }

    /// Set a custom prefix for shard names (default: "shard_").
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.shard_prefix = prefix.into();
        self
    }

    /// Get the shard count.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Extract a numeric value from a Value for modulo calculation.
    ///
    /// Truncation on 32-bit platforms is acceptable here since we only need
    /// the value for consistent shard routing via modulo.
    #[allow(clippy::cast_possible_truncation)]
    fn extract_numeric(&self, value: &Value) -> usize {
        match value {
            Value::BigInt(n) => (*n).unsigned_abs() as usize,
            Value::Int(n) => (*n).unsigned_abs() as usize,
            Value::SmallInt(n) => (*n).unsigned_abs() as usize,
            Value::Text(s) => {
                // Hash the string for non-numeric keys
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                s.hash(&mut hasher);
                hasher.finish() as usize
            }
            _ => 0,
        }
    }
}

impl ShardChooser for ModuloShardChooser {
    fn choose_for_model(&self, shard_key: &Value) -> String {
        let n = self.extract_numeric(shard_key);
        format!("{}{}", self.shard_prefix, n % self.shard_count)
    }

    fn choose_for_query(&self, hints: &QueryHints) -> Vec<String> {
        // If specific shards are targeted, use those
        if let Some(ref targets) = hints.target_shards {
            return targets.clone();
        }

        // If shard key is available, route to specific shard
        if let Some(ref value) = hints.shard_key_value {
            return vec![self.choose_for_model(value)];
        }

        // Default: scatter-gather to all shards
        self.all_shards()
    }

    fn all_shards(&self) -> Vec<String> {
        (0..self.shard_count)
            .map(|i| format!("{}{}", self.shard_prefix, i))
            .collect()
    }
}

/// A sharded connection pool that routes operations to the correct shard.
///
/// `ShardedPool` wraps multiple `Pool` instances, one per shard, and uses
/// a `ShardChooser` to determine which shard to use for each operation.
///
/// # Example
///
/// ```rust,ignore
/// // Create pools for each shard
/// let pool_0 = Pool::new(PoolConfig::new(10));
/// let pool_1 = Pool::new(PoolConfig::new(10));
///
/// // Create sharded pool with modulo chooser
/// let chooser = ModuloShardChooser::new(2);
/// let mut sharded = ShardedPool::new(chooser);
/// sharded.add_shard("shard_0", pool_0);
/// sharded.add_shard("shard_1", pool_1);
///
/// // Acquire connection from specific shard
/// let conn = sharded.acquire_for_model(&cx, &order, factory).await?;
/// ```
pub struct ShardedPool<C: Connection, S: ShardChooser> {
    shards: HashMap<String, Pool<C>>,
    chooser: Arc<S>,
}

impl<C: Connection, S: ShardChooser> ShardedPool<C, S> {
    /// Create a new sharded pool with the given shard chooser.
    pub fn new(chooser: S) -> Self {
        Self {
            shards: HashMap::new(),
            chooser: Arc::new(chooser),
        }
    }

    /// Add a shard to the pool.
    ///
    /// # Arguments
    ///
    /// * `name` - The shard name (must match names returned by the chooser)
    /// * `pool` - The connection pool for this shard
    pub fn add_shard(&mut self, name: impl Into<String>, pool: Pool<C>) {
        self.shards.insert(name.into(), pool);
    }

    /// Add a shard with a new pool created from the given config.
    pub fn add_shard_with_config(&mut self, name: impl Into<String>, config: PoolConfig) {
        self.shards.insert(name.into(), Pool::new(config));
    }

    /// Get a reference to the shard chooser.
    pub fn chooser(&self) -> &S {
        &self.chooser
    }

    /// Get a reference to a specific shard's pool.
    pub fn get_shard(&self, name: &str) -> Option<&Pool<C>> {
        self.shards.get(name)
    }

    /// Get all shard names.
    pub fn shard_names(&self) -> Vec<String> {
        self.shards.keys().cloned().collect()
    }

    /// Get the number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Check if a shard exists.
    pub fn has_shard(&self, name: &str) -> bool {
        self.shards.contains_key(name)
    }

    /// Choose the shard for a model based on its shard key.
    ///
    /// Returns the shard name. Use this when you need to know the shard
    /// without acquiring a connection.
    #[allow(clippy::result_large_err)]
    pub fn choose_for_model<M: Model>(&self, model: &M) -> Result<String, Error> {
        let shard_key = model.shard_key_value().ok_or_else(|| {
            Error::Pool(PoolError {
                kind: PoolErrorKind::Config,
                message: format!(
                    "Model {} has no shard key defined; add #[sqlmodel(shard_key = \"field\")]",
                    M::TABLE_NAME
                ),
                source: None,
            })
        })?;
        Ok(self.chooser.choose_for_model(&shard_key))
    }

    /// Choose shards for a query based on hints.
    pub fn choose_for_query(&self, hints: &QueryHints) -> Vec<String> {
        self.chooser.choose_for_query(hints)
    }

    /// Acquire a connection from the shard determined by the model's shard key.
    ///
    /// # Arguments
    ///
    /// * `cx` - The async context
    /// * `model` - The model instance (must have a shard key)
    /// * `factory` - Connection factory function
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The model has no shard key
    /// - The determined shard doesn't exist
    /// - Connection acquisition fails
    pub async fn acquire_for_model<M, F, Fut>(
        &self,
        cx: &Cx,
        model: &M,
        factory: F,
    ) -> Outcome<PooledConnection<C>, Error>
    where
        M: Model,
        F: Fn() -> Fut,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        let shard_name = match self.choose_for_model(model) {
            Ok(name) => name,
            Err(e) => return Outcome::Err(e),
        };

        self.acquire_from_shard(cx, &shard_name, factory).await
    }

    /// Acquire a connection from a specific shard by name.
    ///
    /// # Arguments
    ///
    /// * `cx` - The async context
    /// * `shard_name` - The name of the shard to acquire from
    /// * `factory` - Connection factory function
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The shard doesn't exist
    /// - Connection acquisition fails
    pub async fn acquire_from_shard<F, Fut>(
        &self,
        cx: &Cx,
        shard_name: &str,
        factory: F,
    ) -> Outcome<PooledConnection<C>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        let Some(pool) = self.shards.get(shard_name) else {
            return Outcome::Err(Error::Pool(PoolError {
                kind: PoolErrorKind::Config,
                message: format!(
                    "shard '{}' not found; available shards: {:?}",
                    shard_name,
                    self.shard_names()
                ),
                source: None,
            }));
        };

        pool.acquire(cx, factory).await
    }

    /// Acquire connections from multiple shards for scatter-gather queries.
    ///
    /// Returns a map of shard name to pooled connection for each successfully
    /// acquired connection. Failed acquisitions are logged but don't fail the
    /// entire operation.
    ///
    /// # Arguments
    ///
    /// * `cx` - The async context
    /// * `hints` - Query routing hints
    /// * `factory` - Connection factory function
    #[allow(clippy::result_large_err)] // Error is the workspace's rich public error type.
    pub async fn acquire_for_query<F, Fut>(
        &self,
        cx: &Cx,
        hints: &QueryHints,
        factory: F,
    ) -> Result<HashMap<String, PooledConnection<C>>, Error>
    where
        F: Fn() -> Fut + Clone,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        let target_shards = self.choose_for_query(hints);
        let mut connections = HashMap::new();

        for shard_name in target_shards {
            match self
                .acquire_from_shard(cx, &shard_name, factory.clone())
                .await
            {
                Outcome::Ok(conn) => {
                    connections.insert(shard_name, conn);
                }
                Outcome::Err(e) => {
                    tracing::warn!(shard = %shard_name, error = %e, "Failed to acquire connection from shard");
                }
                Outcome::Cancelled(reason) => {
                    tracing::debug!(shard = %shard_name, reason = ?reason, "Cancelled while acquiring from shard");
                }
                Outcome::Panicked(info) => {
                    tracing::error!(shard = %shard_name, panic = ?info, "Panic while acquiring from shard");
                }
            }
        }

        if connections.is_empty() {
            return Err(Error::Pool(PoolError {
                kind: PoolErrorKind::Exhausted,
                message: "failed to acquire connection from any shard".to_string(),
                source: None,
            }));
        }

        Ok(connections)
    }

    /// Close all shards.
    pub fn close(&self) {
        for pool in self.shards.values() {
            pool.close();
        }
    }

    /// Check if all shards are closed.
    pub fn is_closed(&self) -> bool {
        self.shards.values().all(|p| p.is_closed())
    }

    /// Get aggregate statistics across all shards.
    pub fn stats(&self) -> ShardedPoolStats {
        let mut total = ShardedPoolStats::default();

        for (name, pool) in &self.shards {
            let shard_stats = pool.stats();
            total.per_shard.insert(name.clone(), shard_stats.clone());
            total.total_connections += shard_stats.total_connections;
            total.idle_connections += shard_stats.idle_connections;
            total.active_connections += shard_stats.active_connections;
            total.pending_requests += shard_stats.pending_requests;
            total.connections_created += shard_stats.connections_created;
            total.connections_closed += shard_stats.connections_closed;
            total.acquires += shard_stats.acquires;
            total.timeouts += shard_stats.timeouts;
        }

        total.shard_count = self.shards.len();
        total
    }
}

/// Aggregate statistics for a sharded pool.
#[derive(Debug, Clone, Default)]
pub struct ShardedPoolStats {
    /// Number of shards.
    pub shard_count: usize,
    /// Per-shard statistics.
    pub per_shard: HashMap<String, crate::PoolStats>,
    /// Total connections across all shards.
    pub total_connections: usize,
    /// Idle connections across all shards.
    pub idle_connections: usize,
    /// Active connections across all shards.
    pub active_connections: usize,
    /// Pending requests across all shards.
    pub pending_requests: usize,
    /// Total connections created across all shards.
    pub connections_created: u64,
    /// Total connections closed across all shards.
    pub connections_closed: u64,
    /// Total acquires across all shards.
    pub acquires: u64,
    /// Total timeouts across all shards.
    pub timeouts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_hints_builder() {
        let hints = QueryHints::new()
            .target(vec!["shard_0".to_string()])
            .with_shard_key(Value::BigInt(42))
            .query_type("select");

        assert_eq!(hints.target_shards, Some(vec!["shard_0".to_string()]));
        assert_eq!(hints.shard_key_value, Some(Value::BigInt(42)));
        assert_eq!(hints.query_type, Some("select".to_string()));
    }

    #[test]
    fn test_query_hints_scatter_gather() {
        let hints = QueryHints::new().scatter_gather();
        assert!(hints.scatter_gather);
    }

    #[test]
    fn test_modulo_shard_chooser_new() {
        let chooser = ModuloShardChooser::new(4);
        assert_eq!(chooser.shard_count(), 4);
    }

    #[test]
    fn test_modulo_shard_chooser_with_prefix() {
        let chooser = ModuloShardChooser::new(3).with_prefix("db_");
        assert_eq!(
            chooser.choose_for_model(&Value::BigInt(0)),
            "db_0".to_string()
        );
        assert_eq!(
            chooser.choose_for_model(&Value::BigInt(1)),
            "db_1".to_string()
        );
    }

    #[test]
    fn test_modulo_shard_chooser_choose_for_model() {
        let chooser = ModuloShardChooser::new(3);

        assert_eq!(chooser.choose_for_model(&Value::BigInt(0)), "shard_0");
        assert_eq!(chooser.choose_for_model(&Value::BigInt(1)), "shard_1");
        assert_eq!(chooser.choose_for_model(&Value::BigInt(2)), "shard_2");
        assert_eq!(chooser.choose_for_model(&Value::BigInt(3)), "shard_0");
        assert_eq!(chooser.choose_for_model(&Value::BigInt(100)), "shard_1");
    }

    #[test]
    fn test_modulo_shard_chooser_int_types() {
        let chooser = ModuloShardChooser::new(2);

        assert_eq!(chooser.choose_for_model(&Value::Int(5)), "shard_1");
        assert_eq!(chooser.choose_for_model(&Value::SmallInt(4)), "shard_0");
    }

    #[test]
    fn test_modulo_shard_chooser_negative_values() {
        let chooser = ModuloShardChooser::new(3);

        // Negative values should use absolute value
        assert_eq!(chooser.choose_for_model(&Value::BigInt(-1)), "shard_1");
        assert_eq!(chooser.choose_for_model(&Value::BigInt(-3)), "shard_0");
    }

    #[test]
    fn test_modulo_shard_chooser_string_hash() {
        let chooser = ModuloShardChooser::new(3);

        // Strings should be hashed consistently
        let shard1 = chooser.choose_for_model(&Value::Text("user_abc".to_string()));
        let shard2 = chooser.choose_for_model(&Value::Text("user_abc".to_string()));
        assert_eq!(shard1, shard2);

        // Different strings may hash to same or different shards
        let _ = chooser.choose_for_model(&Value::Text("user_xyz".to_string()));
    }

    #[test]
    fn test_modulo_shard_chooser_all_shards() {
        let chooser = ModuloShardChooser::new(3);
        let all = chooser.all_shards();

        assert_eq!(all.len(), 3);
        assert!(all.contains(&"shard_0".to_string()));
        assert!(all.contains(&"shard_1".to_string()));
        assert!(all.contains(&"shard_2".to_string()));
    }

    #[test]
    fn test_modulo_shard_chooser_choose_for_query_with_key() {
        let chooser = ModuloShardChooser::new(3);
        let hints = QueryHints::new().with_shard_key(Value::BigInt(5));

        let shards = chooser.choose_for_query(&hints);
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0], "shard_2"); // 5 % 3 = 2
    }

    #[test]
    fn test_modulo_shard_chooser_choose_for_query_scatter() {
        let chooser = ModuloShardChooser::new(3);
        let hints = QueryHints::new().scatter_gather();

        let shards = chooser.choose_for_query(&hints);
        assert_eq!(shards.len(), 3);
    }

    #[test]
    fn test_modulo_shard_chooser_choose_for_query_target() {
        let chooser = ModuloShardChooser::new(3);
        let hints = QueryHints::new().target(vec!["shard_1".to_string()]);

        let shards = chooser.choose_for_query(&hints);
        assert_eq!(shards, vec!["shard_1"]);
    }

    #[test]
    fn test_sharded_pool_stats_default() {
        let stats = ShardedPoolStats::default();
        assert_eq!(stats.shard_count, 0);
        assert_eq!(stats.total_connections, 0);
        assert!(stats.per_shard.is_empty());
    }

    #[test]
    #[should_panic(expected = "shard_count must be greater than 0")]
    fn test_modulo_shard_chooser_zero_shards_panics() {
        let _ = ModuloShardChooser::new(0);
    }
}
