//! Read replica routing for connection pools.
//!
//! Provides `ReplicaPool` which routes read queries to replica databases
//! and write queries to the primary database.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Connection, Error};

use crate::{Pool, PooledConnection};

/// Strategy for selecting which replica to use for reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaStrategy {
    /// Rotate through replicas in order.
    RoundRobin,
    /// Pick a random replica each time.
    Random,
}

/// A pool that routes reads to replicas and writes to a primary.
///
/// # Example
///
/// ```ignore
/// let primary = Pool::new(primary_config);
/// let replica1 = Pool::new(replica1_config);
/// let replica2 = Pool::new(replica2_config);
///
/// let pool = ReplicaPool::new(primary, vec![replica1, replica2]);
///
/// // Reads go to replicas (factory provided at acquire time)
/// let conn = pool.acquire_read(&cx, || connect_replica()).await?;
///
/// // Writes go to primary
/// let conn = pool.acquire_write(&cx, || connect_primary()).await?;
/// ```
pub struct ReplicaPool<C: Connection> {
    /// Primary pool (for writes and explicit primary reads).
    primary: Pool<C>,
    /// Replica pools (for reads).
    replicas: Vec<Pool<C>>,
    /// Selection strategy for replicas.
    strategy: ReplicaStrategy,
    /// Counter for round-robin selection.
    round_robin_counter: AtomicUsize,
}

impl<C: Connection> ReplicaPool<C> {
    /// Create a new replica pool with round-robin strategy.
    pub fn new(primary: Pool<C>, replicas: Vec<Pool<C>>) -> Self {
        Self {
            primary,
            replicas,
            strategy: ReplicaStrategy::RoundRobin,
            round_robin_counter: AtomicUsize::new(0),
        }
    }

    /// Create a new replica pool with a specific strategy.
    pub fn with_strategy(
        primary: Pool<C>,
        replicas: Vec<Pool<C>>,
        strategy: ReplicaStrategy,
    ) -> Self {
        Self {
            primary,
            replicas,
            strategy,
            round_robin_counter: AtomicUsize::new(0),
        }
    }

    /// Acquire a connection for read operations.
    ///
    /// If replicas are available, selects one based on the configured strategy.
    /// Falls back to the primary if no replicas are configured.
    pub async fn acquire_read<F, Fut>(
        &self,
        cx: &Cx,
        factory: F,
    ) -> Outcome<PooledConnection<C>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        if self.replicas.is_empty() {
            return self.primary.acquire(cx, factory).await;
        }

        let idx = self.select_replica();
        self.replicas[idx].acquire(cx, factory).await
    }

    /// Acquire a connection for write operations (always uses primary).
    pub async fn acquire_write<F, Fut>(
        &self,
        cx: &Cx,
        factory: F,
    ) -> Outcome<PooledConnection<C>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        self.primary.acquire(cx, factory).await
    }

    /// Acquire a connection from the primary (for read-after-write consistency).
    pub async fn acquire_primary<F, Fut>(
        &self,
        cx: &Cx,
        factory: F,
    ) -> Outcome<PooledConnection<C>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Outcome<C, Error>>,
    {
        self.primary.acquire(cx, factory).await
    }

    /// Get a reference to the primary pool.
    pub fn primary(&self) -> &Pool<C> {
        &self.primary
    }

    /// Get the replica pools.
    pub fn replicas(&self) -> &[Pool<C>] {
        &self.replicas
    }

    /// Get the number of replicas.
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// Get the current strategy.
    pub fn strategy(&self) -> ReplicaStrategy {
        self.strategy
    }

    fn select_replica(&self) -> usize {
        match self.strategy {
            ReplicaStrategy::RoundRobin => {
                let idx = self.round_robin_counter.fetch_add(1, Ordering::Relaxed);
                idx % self.replicas.len()
            }
            ReplicaStrategy::Random => {
                // Mix counter bits to approximate uniform distribution without
                // pulling in a random number generator dependency.
                let seq = self.round_robin_counter.fetch_add(1, Ordering::Relaxed);
                // Multiplicative hash using golden ratio constant.
                // Use 32-bit mixing and cast to usize for portability across architectures.
                #[allow(clippy::cast_possible_truncation)]
                let seq32 = seq as u32;
                let mixed = seq32.wrapping_mul(2_654_435_761_u32);
                (mixed as usize) % self.replicas.len()
            }
        }
    }
}

impl<C: Connection> std::fmt::Debug for ReplicaPool<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaPool")
            .field("primary", &"Pool { .. }")
            .field("replicas", &self.replicas.len())
            .field("strategy", &self.strategy)
            .field(
                "round_robin_counter",
                &self.round_robin_counter.load(Ordering::Relaxed),
            )
            .finish()
    }
}
