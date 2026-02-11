//! Deterministic test harness for reproducible E2E and PTY testing.
//!
//! Provides shared utilities for deterministic clocks, seeded randomness,
//! stable ID generation, and reproducible environment capture. Test suites
//! use these primitives so that CI failures can be reproduced with a single
//! seed value.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use mcp_agent_mail_core::test_harness::{Harness, HarnessConfig};
//!
//! let h = Harness::new(HarnessConfig { seed: 42, ..Default::default() });
//! let ts = h.clock.now_micros();       // deterministic timestamp
//! let id = h.ids.next_id();            // stable sequential ID
//! let val = h.rng(|r| r.next_u64());   // seeded random
//! ```
//!
//! # Reproduction
//!
//! Every harness instance captures a [`ReproContext`] that can be serialized
//! to JSON. CI scripts embed this in test artifacts so failures can be
//! replayed:
//!
//! ```bash
//! HARNESS_SEED=42 cargo test --test my_suite
//! ```

#![allow(
    clippy::missing_const_for_fn,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

// ── Seeded PRNG (xorshift64) ────────────────────────────────────────────

/// Deterministic pseudo-random number generator (xorshift64).
///
/// Produces a reproducible sequence given the same seed. Thread-safe
/// when wrapped in a `Mutex` (see [`Harness::rng`]).
#[derive(Debug, Clone)]
pub struct Rng64 {
    state: u64,
}

impl Rng64 {
    /// Create a new PRNG with the given seed. Zero seeds are remapped to
    /// a fixed non-zero value to avoid the xorshift degenerate case.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x517c_c1b7_2722_0a95
            } else {
                seed
            },
        }
    }

    /// Advance the state and return the next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Return a value in `[0, bound)`. Returns 0 when `bound == 0`.
    pub fn next_bounded(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next_u64() % bound
    }

    /// Return a value in `[lo, hi)`. Panics if `lo >= hi`.
    pub fn next_range(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo < hi, "next_range requires lo < hi");
        lo + self.next_bounded(hi - lo)
    }

    /// Pick a random element from a slice.
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        assert!(!items.is_empty(), "choose requires non-empty slice");
        let idx = self.next_bounded(items.len() as u64) as usize;
        &items[idx]
    }

    /// Derive a child RNG with a new seed based on current state + discriminator.
    #[must_use]
    pub fn fork(&mut self, discriminator: u64) -> Self {
        Self::new(self.next_u64().wrapping_add(discriminator))
    }
}

// ── Deterministic Clock ─────────────────────────────────────────────────

/// A deterministic clock that produces predictable, monotonically
/// increasing timestamps from a configurable base and step size.
///
/// Each call to [`now_micros`](DeterministicClock::now_micros) advances
/// the internal counter by `step_micros`, ensuring reproducible ordering
/// across test runs regardless of wall-clock timing.
#[derive(Debug)]
pub struct DeterministicClock {
    /// Current timestamp in microseconds since epoch.
    current: AtomicI64,
    /// How much to advance per `now_micros()` call.
    step_micros: i64,
}

impl DeterministicClock {
    /// Create a clock starting at `base_micros` with the given step size.
    ///
    /// A typical base is `1_704_067_200_000_000` (2024-01-01 00:00:00 UTC).
    #[must_use]
    pub const fn new(base_micros: i64, step_micros: i64) -> Self {
        Self {
            current: AtomicI64::new(base_micros),
            step_micros,
        }
    }

    /// Return the current timestamp and advance by `step_micros`.
    pub fn now_micros(&self) -> i64 {
        self.current.fetch_add(self.step_micros, Ordering::Relaxed)
    }

    /// Peek at the current timestamp without advancing.
    pub fn peek_micros(&self) -> i64 {
        self.current.load(Ordering::Relaxed)
    }

    /// Manually set the current timestamp.
    pub fn set_micros(&self, micros: i64) {
        self.current.store(micros, Ordering::Relaxed);
    }

    /// Advance by a specific amount (not the default step).
    pub fn advance(&self, micros: i64) {
        self.current.fetch_add(micros, Ordering::Relaxed);
    }
}

/// Default: 2024-01-01 00:00:00 UTC, 1-second steps.
impl Default for DeterministicClock {
    fn default() -> Self {
        Self::new(1_704_067_200_000_000, 1_000_000)
    }
}

// ── Stable ID Generator ─────────────────────────────────────────────────

/// Produces stable, monotonically increasing IDs from a configurable base.
///
/// Unlike auto-increment database IDs, these are fully deterministic
/// and independent of insertion order or database state.
#[derive(Debug)]
pub struct StableIdGen {
    counter: AtomicI64,
}

impl StableIdGen {
    /// Create a generator starting at `base`.
    #[must_use]
    pub const fn new(base: i64) -> Self {
        Self {
            counter: AtomicI64::new(base),
        }
    }

    /// Return the next ID and advance the counter.
    pub fn next_id(&self) -> i64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Peek at the next ID without consuming it.
    pub fn peek(&self) -> i64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Reset the counter to a specific value.
    pub fn reset(&self, base: i64) {
        self.counter.store(base, Ordering::Relaxed);
    }
}

impl Default for StableIdGen {
    fn default() -> Self {
        Self::new(1)
    }
}

// ── Reproduction Context ────────────────────────────────────────────────

/// Captures all parameters needed to reproduce a test run.
///
/// Serialize this to JSON and embed in CI artifacts so failures can be
/// replayed by setting `HARNESS_SEED` and other env vars.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReproContext {
    /// The seed used for this test run.
    pub seed: u64,
    /// Clock base timestamp (microseconds since epoch).
    pub clock_base_micros: i64,
    /// Clock step size (microseconds per tick).
    pub clock_step_micros: i64,
    /// ID generator starting value.
    pub id_base: i64,
    /// Test name or suite identifier.
    pub test_name: String,
    /// ISO-8601 wall-clock time when the harness was created.
    pub created_at: String,
    /// Rust target triple.
    pub target: String,
    /// Extra key-value pairs for suite-specific parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(String, String)>,
}

impl ReproContext {
    /// Format a single-line reproduction command.
    #[must_use]
    pub fn repro_command(&self) -> String {
        let mut parts = vec![format!("HARNESS_SEED={}", self.seed)];
        for (k, v) in &self.extra {
            parts.push(format!("{k}={v}"));
        }
        parts.push(format!("cargo test {}", self.test_name));
        parts.join(" ")
    }
}

// ── Harness Configuration ───────────────────────────────────────────────

/// Configuration for creating a [`Harness`].
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Seed for the PRNG. Default: read from `HARNESS_SEED` env var, or 0.
    pub seed: u64,
    /// Base timestamp for the deterministic clock (microseconds since epoch).
    /// Default: 2024-01-01 00:00:00 UTC.
    pub clock_base_micros: i64,
    /// Step size for the deterministic clock (microseconds per tick).
    /// Default: `1_000_000` (1 second).
    pub clock_step_micros: i64,
    /// Starting value for the stable ID generator. Default: 1.
    pub id_base: i64,
    /// Test name for reproduction context. Default: empty.
    pub test_name: String,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        let seed = std::env::var("HARNESS_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Self {
            seed,
            clock_base_micros: 1_704_067_200_000_000, // 2024-01-01T00:00:00Z
            clock_step_micros: 1_000_000, // 1 second
            id_base: 1,
            test_name: String::new(),
        }
    }
}

// ── Harness ─────────────────────────────────────────────────────────────

/// Deterministic test harness bundling clock, IDs, RNG, and reproduction
/// context into a single reusable object.
///
/// Thread-safe: the RNG is wrapped in a `Mutex`, while clock and ID
/// generator use atomics.
pub struct Harness {
    /// Deterministic clock for timestamp generation.
    pub clock: DeterministicClock,
    /// Stable ID generator.
    pub ids: StableIdGen,
    /// Reproduction context for CI artifact embedding.
    pub repro: ReproContext,
    /// Operation counter for tracking how many actions were performed.
    pub ops: AtomicU64,
    rng: Mutex<Rng64>,
}

impl Harness {
    /// Create a new harness from the given configuration.
    #[must_use]
    pub fn new(config: HarnessConfig) -> Self {
        let repro = ReproContext {
            seed: config.seed,
            clock_base_micros: config.clock_base_micros,
            clock_step_micros: config.clock_step_micros,
            id_base: config.id_base,
            test_name: config.test_name.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            target: std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string()),
            extra: Vec::new(),
        };

        Self {
            clock: DeterministicClock::new(config.clock_base_micros, config.clock_step_micros),
            ids: StableIdGen::new(config.id_base),
            rng: Mutex::new(Rng64::new(config.seed)),
            repro,
            ops: AtomicU64::new(0),
        }
    }

    /// Create a harness with defaults, reading seed from `HARNESS_SEED`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(HarnessConfig::default())
    }

    /// Create a harness with a specific seed and test name.
    #[must_use]
    pub fn with_seed(seed: u64, test_name: &str) -> Self {
        Self::new(HarnessConfig {
            seed,
            test_name: test_name.to_string(),
            ..Default::default()
        })
    }

    /// Lock the RNG and call the provided closure with mutable access.
    ///
    /// ```rust,ignore
    /// let val = harness.rng(|rng| rng.next_bounded(100));
    /// ```
    pub fn rng<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Rng64) -> R,
    {
        let mut guard = self.rng.lock().expect("harness rng lock");
        f(&mut guard)
    }

    /// Derive a child RNG for a worker thread. The discriminator should
    /// be unique per worker (e.g., thread index).
    pub fn fork_rng(&self, discriminator: u64) -> Rng64 {
        self.rng(|rng| rng.fork(discriminator))
    }

    /// Record an operation (increment ops counter) and return the count.
    pub fn record_op(&self) -> u64 {
        self.ops.fetch_add(1, Ordering::Relaxed)
    }

    /// Add an extra key-value pair to the reproduction context.
    pub fn add_extra(&mut self, key: &str, value: &str) {
        self.repro.extra.push((key.to_owned(), value.to_owned()));
    }

    /// Serialize the reproduction context to pretty JSON.
    ///
    /// # Errors
    /// Returns `Err` if serialization fails (should not happen in practice).
    pub fn repro_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.repro)
    }

    /// Write the reproduction context to a file.
    ///
    /// # Errors
    /// Returns `Err` on I/O failure.
    pub fn write_repro(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = self.repro_json().map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Generate a deterministic agent name from the harness RNG.
    ///
    /// Uses the project's `VALID_ADJECTIVES` and `VALID_NOUNS` lists.
    pub fn agent_name(&self) -> String {
        self.rng(|rng| {
            let adj = crate::VALID_ADJECTIVES;
            let noun = crate::VALID_NOUNS;
            let a = adj[rng.next_bounded(adj.len() as u64) as usize];
            let n = noun[rng.next_bounded(noun.len() as u64) as usize];
            format!("{}{}", capitalize(a), capitalize(n))
        })
    }

    /// Generate N deterministic agent names (guaranteed unique within the batch).
    pub fn agent_names(&self, n: usize) -> Vec<String> {
        let mut names = Vec::with_capacity(n);
        let mut seen = std::collections::HashSet::with_capacity(n);
        while names.len() < n {
            let name = self.agent_name();
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        names
    }
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("seed", &self.repro.seed)
            .field("clock", &self.clock)
            .field("ids", &self.ids)
            .field("ops", &self.ops.load(Ordering::Relaxed))
            .field("rng", &"<mutex>")
            .finish()
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

// ── Artifact Helpers ────────────────────────────────────────────────────

/// Standard artifact directory under the repo root.
///
/// Returns `{repo_root}/tests/artifacts/{subdir}/{timestamp}_{pid}/`.
/// Creates the directory if it doesn't exist.
///
/// # Errors
/// Returns `Err` if directory creation fails.
pub fn artifact_dir(subdir: &str) -> std::io::Result<std::path::PathBuf> {
    let ts = chrono::Utc::now()
        .format("%Y%m%d_%H%M%S%.3fZ")
        .to_string();
    let pid = std::process::id();
    // Navigate from any crate's `CARGO_MANIFEST_DIR` up to repo root.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .unwrap_or_else(|| std::path::Path::new("."));
    let dir = root.join(format!("tests/artifacts/{subdir}/{ts}_{pid}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Write a JSON artifact file and print its path to stderr.
///
/// # Errors
/// Returns `Err` on serialization or I/O failure.
pub fn write_artifact(
    dir: &std::path::Path,
    filename: &str,
    value: &impl serde::Serialize,
) -> std::io::Result<()> {
    let json =
        serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let path = dir.join(filename);
    std::fs::write(&path, json)?;
    eprintln!("artifact: {}", path.display());
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_deterministic() {
        let mut a = Rng64::new(42);
        let mut b = Rng64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_zero_seed_remapped() {
        let mut a = Rng64::new(0);
        assert_ne!(a.next_u64(), 0);
    }

    #[test]
    fn rng_bounded() {
        let mut rng = Rng64::new(1);
        for _ in 0..1000 {
            let val = rng.next_bounded(10);
            assert!(val < 10);
        }
    }

    #[test]
    fn rng_range() {
        let mut rng = Rng64::new(7);
        for _ in 0..1000 {
            let val = rng.next_range(5, 15);
            assert!(val >= 5);
            assert!(val < 15);
        }
    }

    #[test]
    fn rng_choose() {
        let items = ["a", "b", "c"];
        let mut rng = Rng64::new(99);
        for _ in 0..100 {
            let pick = rng.choose(&items);
            assert!(items.contains(pick));
        }
    }

    #[test]
    fn rng_fork_produces_different_sequence() {
        let mut parent = Rng64::new(42);
        let mut child = parent.fork(1);
        let p_vals: Vec<u64> = (0..10).map(|_| parent.next_u64()).collect();
        let c_vals: Vec<u64> = (0..10).map(|_| child.next_u64()).collect();
        assert_ne!(p_vals, c_vals);
    }

    #[test]
    fn clock_deterministic() {
        let clock = DeterministicClock::new(1_000_000, 500);
        assert_eq!(clock.now_micros(), 1_000_000);
        assert_eq!(clock.now_micros(), 1_000_500);
        assert_eq!(clock.now_micros(), 1_001_000);
    }

    #[test]
    fn clock_peek_no_advance() {
        let clock = DeterministicClock::new(100, 10);
        assert_eq!(clock.peek_micros(), 100);
        assert_eq!(clock.peek_micros(), 100);
        assert_eq!(clock.now_micros(), 100);
        assert_eq!(clock.peek_micros(), 110);
    }

    #[test]
    fn clock_set_and_advance() {
        let clock = DeterministicClock::default();
        clock.set_micros(5_000_000);
        assert_eq!(clock.peek_micros(), 5_000_000);
        clock.advance(2_000_000);
        assert_eq!(clock.peek_micros(), 7_000_000);
    }

    #[test]
    fn id_gen_sequential() {
        let id_gen = StableIdGen::new(100);
        assert_eq!(id_gen.next_id(), 100);
        assert_eq!(id_gen.next_id(), 101);
        assert_eq!(id_gen.next_id(), 102);
    }

    #[test]
    fn id_gen_reset() {
        let id_gen = StableIdGen::new(1);
        id_gen.next_id();
        id_gen.next_id();
        id_gen.reset(50);
        assert_eq!(id_gen.next_id(), 50);
    }

    #[test]
    fn harness_creation() {
        let h = Harness::with_seed(42, "test_harness_creation");
        assert_eq!(h.repro.seed, 42);
        assert_eq!(h.repro.test_name, "test_harness_creation");
    }

    #[test]
    fn harness_rng_access() {
        let h = Harness::with_seed(42, "test_rng");
        let a = h.rng(Rng64::next_u64);
        let b = h.rng(Rng64::next_u64);
        assert_ne!(a, b);
    }

    #[test]
    fn harness_fork_rng() {
        let h = Harness::with_seed(42, "test_fork");
        let mut r1 = h.fork_rng(1);
        let mut r2 = h.fork_rng(2);
        let v1: Vec<u64> = (0..5).map(|_| r1.next_u64()).collect();
        let v2: Vec<u64> = (0..5).map(|_| r2.next_u64()).collect();
        assert_ne!(v1, v2);
    }

    #[test]
    fn harness_ops_counter() {
        let h = Harness::with_seed(0, "test_ops");
        assert_eq!(h.record_op(), 0);
        assert_eq!(h.record_op(), 1);
        assert_eq!(h.record_op(), 2);
    }

    #[test]
    fn harness_agent_name_valid() {
        let h = Harness::with_seed(42, "test_names");
        for _ in 0..20 {
            let name = h.agent_name();
            assert!(
                crate::is_valid_agent_name(&name),
                "generated invalid name: {name}"
            );
        }
    }

    #[test]
    fn harness_agent_names_unique() {
        let h = Harness::with_seed(42, "test_unique_names");
        let names = h.agent_names(10);
        assert_eq!(names.len(), 10);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), 10, "names not unique: {names:?}");
    }

    #[test]
    fn harness_repro_json() {
        let h = Harness::with_seed(42, "test_repro");
        let json = h.repro_json().unwrap();
        assert!(json.contains("\"seed\": 42"));
        assert!(json.contains("test_repro"));
    }

    #[test]
    fn harness_repro_command() {
        let mut h = Harness::with_seed(42, "my_test");
        h.add_extra("SOAK_PROJECTS", "10");
        let cmd = h.repro.repro_command();
        assert!(cmd.contains("HARNESS_SEED=42"));
        assert!(cmd.contains("SOAK_PROJECTS=10"));
        assert!(cmd.contains("cargo test my_test"));
    }

    #[test]
    fn harness_deterministic_across_runs() {
        let h1 = Harness::with_seed(999, "repro_test");
        let h2 = Harness::with_seed(999, "repro_test");

        let ts1: Vec<i64> = (0..5).map(|_| h1.clock.now_micros()).collect();
        let ts2: Vec<i64> = (0..5).map(|_| h2.clock.now_micros()).collect();
        assert_eq!(ts1, ts2);

        let ids1: Vec<i64> = (0..5).map(|_| h1.ids.next_id()).collect();
        let ids2: Vec<i64> = (0..5).map(|_| h2.ids.next_id()).collect();
        assert_eq!(ids1, ids2);

        let rng1: Vec<u64> = (0..5).map(|_| h1.rng(Rng64::next_u64)).collect();
        let rng2: Vec<u64> = (0..5).map(|_| h2.rng(Rng64::next_u64)).collect();
        assert_eq!(rng1, rng2);
    }

    #[test]
    fn write_and_read_repro_artifact() {
        let h = Harness::with_seed(42, "artifact_test");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repro.json");
        h.write_repro(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let ctx: ReproContext = serde_json::from_str(&content).unwrap();
        assert_eq!(ctx.seed, 42);
        assert_eq!(ctx.test_name, "artifact_test");
    }
}
