//! `Model2Vec` embedding backend for ultra-fast semantic search.
//!
//! Thin wrapper around [`frankensearch::Model2VecEmbedder`] that adds
//! agent-mail-specific model search paths and preserves the sync
//! `TwoTierEmbedder` interface.
//!
//! # Supported Models
//!
//! - `potion-retrieval-32M` (256 dims, ~32MB)
//! - `potion-multilingual-128M` (256 dims, ~128MB)

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use frankensearch::Embedder as _;

use crate::search_error::{SearchError, SearchResult};
use crate::search_fs_bridge::map_fs_error;
use crate::search_two_tier::TwoTierEmbedder;

/// Model name constant for potion-retrieval-32M.
pub const MODEL_POTION_32M: &str = "potion-retrieval-32M";

/// Model name constant for potion-multilingual-128M (our fast tier choice).
pub const MODEL_POTION_128M: &str = "potion-multilingual-128M";

/// `Model2Vec` embedder — thin wrapper around `frankensearch::Model2VecEmbedder`.
///
/// Delegates all embedding logic to frankensearch while preserving the
/// `TwoTierEmbedder` sync interface and agent-mail-specific model search paths.
pub struct Model2VecEmbedder {
    inner: frankensearch::Model2VecEmbedder,
}

impl std::fmt::Debug for Model2VecEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model2VecEmbedder")
            .field("name", &self.inner.id())
            .field("dimensions", &self.inner.dimension())
            .field("vocab_size", &self.inner.vocab_size())
            .finish_non_exhaustive()
    }
}

impl Model2VecEmbedder {
    /// Load the model from a directory containing model files.
    ///
    /// # Errors
    ///
    /// Returns an error if the model files are missing or invalid.
    pub fn load_from_dir(model_dir: &Path, model_name: &str) -> SearchResult<Self> {
        let inner = frankensearch::Model2VecEmbedder::load_with_name(model_dir, model_name)
            .map_err(map_fs_error)?;

        tracing::info!(
            model = model_name,
            vocab_size = inner.vocab_size(),
            dimensions = inner.dimension(),
            "Model2Vec embedder loaded (via frankensearch)"
        );

        Ok(Self { inner })
    }

    /// Try to load from standard model locations.
    ///
    /// Searches frankensearch standard paths first, then agent-mail-specific
    /// directories. A broken or partially installed cache entry must not hide
    /// a healthy installation in a later location.
    ///
    /// # Errors
    ///
    /// Returns an error with per-location diagnostics if no candidate loads.
    pub fn try_load(model_name: &str) -> SearchResult<Self> {
        let mut candidates = Vec::new();
        if let Some(dir) = frankensearch_embed::find_model_dir(model_name) {
            candidates.push(dir);
        }
        candidates.extend(Self::agent_mail_model_paths(model_name));
        load_first_available(model_name, &candidates, |candidate| {
            Self::load_from_dir(candidate, model_name)
        })
    }

    /// Agent-mail-specific model search paths (not covered by frankensearch).
    fn agent_mail_model_paths(model_name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // mcp-agent-mail data directory
        if let Some(data) = dirs::data_local_dir() {
            paths.push(data.join("mcp-agent-mail").join("models").join(model_name));
        }

        // mcp-agent-mail cache directory
        if let Some(cache) = dirs::cache_dir() {
            paths.push(cache.join("mcp-agent-mail").join("models").join(model_name));
        }

        paths
    }

    /// Get standard model search paths (frankensearch + agent-mail).
    #[must_use]
    pub fn model_search_paths(model_name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // HuggingFace hub cache
        if let Some(cache) = dirs::cache_dir() {
            paths.push(
                cache
                    .join("huggingface")
                    .join("hub")
                    .join(format!("models--minishlab--{model_name}")),
            );
        }

        // Agent-mail-specific paths
        paths.extend(Self::agent_mail_model_paths(model_name));

        paths
    }

    /// Check if a specific model is available.
    #[must_use]
    pub fn is_available(model_name: &str) -> bool {
        Self::try_load(model_name).is_ok()
    }

    /// Get the vocabulary size.
    #[must_use]
    pub const fn vocab_size(&self) -> usize {
        self.inner.vocab_size()
    }

    /// Access the inner frankensearch embedder.
    #[must_use]
    pub const fn as_inner(&self) -> &frankensearch::Model2VecEmbedder {
        &self.inner
    }
}

/// Keep candidate order, but continue after a load error.
///
/// Directory discovery is not proof that a complete, valid model is installed.
fn load_first_available<T>(
    model_name: &str,
    candidates: &[PathBuf],
    mut load: impl FnMut(&Path) -> SearchResult<T>,
) -> SearchResult<T> {
    let mut attempted = Vec::new();
    let mut failures = Vec::new();
    for candidate in candidates {
        if attempted.contains(&candidate) {
            continue;
        }
        attempted.push(candidate);
        match load(candidate) {
            Ok(value) => return Ok(value),
            Err(error) => {
                tracing::debug!(
                    model = model_name,
                    path = %candidate.display(),
                    error = %error,
                    "Model2Vec candidate failed to load"
                );
                failures.push(format!("{}: {error}", candidate.display()));
            }
        }
    }
    let detail = if failures.is_empty() {
        "no model search locations available".to_string()
    } else {
        failures.join("; ")
    };
    Err(SearchError::ModeUnavailable(format!(
        "{model_name} model unavailable: {detail}"
    )))
}

impl TwoTierEmbedder for Model2VecEmbedder {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        if text.is_empty() {
            return Err(SearchError::InvalidQuery("empty text".to_string()));
        }
        self.inner.embed_sync(text).map_err(map_fs_error)
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn id(&self) -> &str {
        self.inner.id()
    }
}

// ────────────────────────────────────────────────────────────────────
// Global auto-initialization
// ────────────────────────────────────────────────────────────────────

const MODEL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// Publish only a successful initialization.
///
/// Negative results expire, while successful values retain a stable address
/// and need no lock on the hot path.
/// Contending callers do not wait behind model I/O; they can use lexical search.
struct RetryableInit<T> {
    value: OnceLock<T>,
    last_attempt: Mutex<Option<Instant>>,
}

impl<T> RetryableInit<T> {
    const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            last_attempt: Mutex::new(None),
        }
    }

    fn get_or_try_init(&self, cooldown: Duration, init: impl FnOnce() -> Option<T>) -> Option<&T> {
        self.get_or_try_init_with_clock(cooldown, Instant::now, init)
    }

    fn get_or_try_init_with_clock(
        &self,
        cooldown: Duration,
        clock: impl Fn() -> Instant,
        init: impl FnOnce() -> Option<T>,
    ) -> Option<&T> {
        if let Some(value) = self.value.get() {
            return Some(value);
        }
        let mut last_attempt = match self.last_attempt.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return self.value.get(),
        };
        // An initializer may have published between the fast path and the lock.
        if let Some(value) = self.value.get() {
            return Some(value);
        }
        let now = clock();
        if last_attempt.is_some_and(|last| now.saturating_duration_since(last) < cooldown) {
            return None;
        }
        // Set before invoking caller code as well, so a panicking initializer
        // cannot permanently wedge the gate or cause an immediate retry storm.
        *last_attempt = Some(now);
        let value = init();
        // Backoff starts at completion, not at the beginning of a slow failed load.
        *last_attempt = Some(clock());
        if let Some(value) = value {
            let _ = self.value.set(value);
        }
        drop(last_attempt);
        self.value.get()
    }
}

/// Global fast embedder instance. Only a successfully loaded model is permanent.
static FAST_EMBEDDER: RetryableInit<Model2VecEmbedder> = RetryableInit::new();

/// Get the global fast embedder, auto-initializing if necessary.
///
/// Returns `None` while no model is available or another caller is loading it.
/// Failed discovery is retried on demand after a 30-second cooldown, allowing
/// models installed after server startup to become available without a restart.
/// A successful model is never replaced, preserving dimensions and vector space
/// for existing indexes and references.
#[must_use]
pub fn get_fast_embedder() -> Option<&'static Model2VecEmbedder> {
    FAST_EMBEDDER.get_or_try_init(MODEL_RETRY_COOLDOWN, || {
        if let Ok(embedder) = Model2VecEmbedder::try_load(MODEL_POTION_128M) {
            tracing::info!(model = MODEL_POTION_128M, "Fast embedder auto-initialized");
            return Some(embedder);
        }

        if let Ok(embedder) = Model2VecEmbedder::try_load(MODEL_POTION_32M) {
            tracing::info!(
                model = MODEL_POTION_32M,
                "Fast embedder auto-initialized (fallback)"
            );
            return Some(embedder);
        }

        tracing::warn!("No fast embedder model available; discovery will retry after cooldown");
        None
    })
}

/// Check if the fast embedder is available.
#[must_use]
pub fn is_fast_embedder_available() -> bool {
    get_fast_embedder().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::mpsc;

    #[test]
    fn broken_cache_does_not_hide_a_healthy_fallback() {
        let candidates = [PathBuf::from("broken-cache"), PathBuf::from("installed")];
        let mut attempted = Vec::new();
        let result = load_first_available("model", &candidates, |path| {
            attempted.push(path.to_path_buf());
            if path == Path::new("installed") {
                Ok(42)
            } else {
                Err(SearchError::ModeUnavailable("incomplete model".into()))
            }
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempted, candidates);
    }

    #[test]
    fn candidate_priority_is_preserved_and_duplicates_are_not_reloaded() {
        let candidates = [
            PathBuf::from("broken"),
            PathBuf::from("broken"),
            PathBuf::from("preferred"),
            PathBuf::from("unused"),
        ];
        let mut attempted = Vec::new();
        let result = load_first_available("model", &candidates, |path| {
            attempted.push(path.to_path_buf());
            if path == Path::new("broken") {
                Err(SearchError::ModeUnavailable("broken".into()))
            } else {
                Ok(path.to_path_buf())
            }
        });
        assert_eq!(result.unwrap(), Path::new("preferred"));
        assert_eq!(
            attempted,
            [PathBuf::from("broken"), PathBuf::from("preferred")]
        );
    }

    #[test]
    fn failed_candidates_preserve_load_diagnostics() {
        let candidates = [PathBuf::from("cache"), PathBuf::from("data")];
        let error = load_first_available::<()>("model", &candidates, |_| {
            Err(SearchError::ModeUnavailable("invalid tensor".into()))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("model"));
        assert!(error.contains("cache"));
        assert!(error.contains("data"));
        assert!(error.contains("invalid tensor"));
        assert!(
            load_first_available::<()>("model", &[], |_| panic!("no candidates"))
                .unwrap_err()
                .to_string()
                .contains("no model search locations")
        );
    }

    #[test]
    fn failed_initialization_retries_after_cooldown_and_publishes_once() {
        let gate = RetryableInit::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(30);
        assert_eq!(
            gate.get_or_try_init_with_clock(cooldown, || now, || None),
            None
        );
        assert_eq!(
            gate.get_or_try_init_with_clock(
                cooldown,
                || now + cooldown / 2,
                || { panic!("negative cache must suppress retry") }
            ),
            None
        );
        let ready = gate
            .get_or_try_init_with_clock(cooldown, || now + cooldown, || Some(42))
            .unwrap();
        assert_eq!(*ready, 42);
        let again = gate
            .get_or_try_init(cooldown, || panic!("must not reload")) // ubs:ignore - test fails on reinitialization
            .unwrap();
        assert!(std::ptr::eq(ready, again));
    }

    #[test]
    fn cooldown_starts_after_a_slow_failure_finishes() {
        let gate = RetryableInit::<usize>::new();
        let start = Instant::now();
        let clock = Cell::new(start);
        let cooldown = Duration::from_secs(30);
        assert!(
            gate.get_or_try_init_with_clock(
                cooldown,
                || clock.get(),
                || {
                    clock.set(start + Duration::from_secs(60));
                    None
                }
            )
            .is_none()
        );
        clock.set(start + Duration::from_secs(89));
        assert!(
            gate.get_or_try_init_with_clock(cooldown, || clock.get(), || panic!("too soon"))
                .is_none()
        );
        clock.set(start + Duration::from_secs(90));
        assert_eq!(
            gate.get_or_try_init_with_clock(cooldown, || clock.get(), || Some(7)),
            Some(&7)
        );
    }

    #[test]
    fn concurrent_callers_fall_back_without_waiting_for_model_io() {
        let gate = RetryableInit::new();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let gate_ref = &gate;
            let loader = scope.spawn(move || {
                gate_ref
                    .get_or_try_init(MODEL_RETRY_COOLDOWN, || {
                        started_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        Some(42)
                    })
                    .copied()
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let (result_tx, result_rx) = mpsc::channel();
            scope.spawn(move || {
                let result = gate_ref
                    .get_or_try_init(MODEL_RETRY_COOLDOWN, || {
                        panic!("another initializer is already running") // ubs:ignore - test fails if a contending reader initializes
                    })
                    .copied();
                result_tx.send(result).unwrap();
            });
            let during_load = result_rx.recv_timeout(Duration::from_secs(1));
            // Release the worker before asserting, including on a regression.
            release_tx.send(()).unwrap();
            assert_eq!(during_load.unwrap(), None);
            assert_eq!(loader.join().unwrap(), Some(42));
        });
        assert_eq!(
            gate.get_or_try_init(MODEL_RETRY_COOLDOWN, || panic!("published")),
            Some(&42)
        );
    }

    #[test]
    fn panicking_and_reentrant_initializers_do_not_wedge_discovery() {
        let gate = RetryableInit::new();
        let now = Instant::now();
        assert!(
            std::panic::catch_unwind(|| {
                gate.get_or_try_init_with_clock(
                    MODEL_RETRY_COOLDOWN,
                    || now,
                    || panic!("model loader panicked"),
                )
            })
            .is_err()
        );
        assert!(
            gate.get_or_try_init_with_clock(
                MODEL_RETRY_COOLDOWN,
                || now,
                || { panic!("panic still starts a cooldown") }
            )
            .is_none()
        );
        let ready = gate.get_or_try_init_with_clock(
            MODEL_RETRY_COOLDOWN,
            || now + MODEL_RETRY_COOLDOWN,
            || {
                assert!(
                    gate.get_or_try_init(MODEL_RETRY_COOLDOWN, || panic!("reentrant"))
                        .is_none()
                );
                Some(9)
            },
        );
        assert_eq!(ready, Some(&9));
    }

    #[test]
    fn test_model_constants() {
        assert_eq!(MODEL_POTION_32M, "potion-retrieval-32M");
        assert_eq!(MODEL_POTION_128M, "potion-multilingual-128M");
    }

    #[test]
    fn test_model_search_paths() {
        let paths = Model2VecEmbedder::model_search_paths(MODEL_POTION_128M);
        assert_ne!(paths, [] as [PathBuf; 0]);

        // Should include HuggingFace cache path
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("huggingface"))
        );
    }

    #[test]
    fn model_search_paths_32m() {
        let paths = Model2VecEmbedder::model_search_paths(MODEL_POTION_32M);
        assert_ne!(paths, [] as [PathBuf; 0]);
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("mcp-agent-mail"))
        );
    }

    #[test]
    fn model_search_paths_custom_name() {
        let paths = Model2VecEmbedder::model_search_paths("my-custom-model");
        assert_ne!(paths, [] as [PathBuf; 0]);
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("my-custom-model"))
        );
    }

    #[test]
    fn is_available_nonexistent_model() {
        assert!(!Model2VecEmbedder::is_available(
            "nonexistent-model-xyz-12345"
        ));
    }

    #[test]
    fn is_fast_embedder_available_no_panic() {
        let _ = is_fast_embedder_available();
    }

    #[test]
    fn get_fast_embedder_no_panic() {
        let _ = get_fast_embedder();
    }

    #[test]
    #[ignore = "requires registered potion bytes; set AM_TEST_POTION_DIR and run explicitly"]
    fn registered_model_recovers_live_context_after_installation() {
        use crate::search_auto_init::{TwoTierAvailability, get_two_tier_context};

        const CHILD: &str = "AM_TEST_MODEL_RECOVERY_CHILD";
        const VERIFIED: &str = "registered model restored live context and normalized embedding";
        let model_source = PathBuf::from(
            std::env::var("AM_TEST_POTION_DIR")
                .expect("AM_TEST_POTION_DIR must name the registered potion model directory"),
        );
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let broken = root.path().join("broken");
            std::fs::create_dir(&broken).unwrap();
            for file in ["tokenizer.json", "model.safetensors"] {
                std::fs::write(broken.join(file), b"incomplete installation").unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "search_model2vec::tests::registered_model_recovers_live_context_after_installation",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AM_TEST_POTION_DIR", model_source.canonicalize().unwrap())
                .env("FRANKENSEARCH_MODEL_DIR", &broken)
                .env_remove("FRANKENSEARCH_DATA_DIR")
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "isolated real-model recovery failed: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            // A misspelled --exact filter must not pass by running zero tests.
            assert!(
                stdout.contains(VERIFIED),
                "child did not execute the recovery proof: {stdout}"
            );
            println!("{VERIFIED}");
            return;
        }

        // The child starts with fresh globals and private discovery directories.
        let context = get_two_tier_context();
        assert_eq!(context.availability(), TwoTierAvailability::None);
        assert!(context.fast_info().is_none());
        assert!(!context.is_available());
        assert!(get_fast_embedder().is_none());
        let installed = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap())
            .join("mcp-agent-mail/models")
            .join(MODEL_POTION_128M);
        std::fs::create_dir_all(&installed).unwrap();
        for file in ["tokenizer.json", "model.safetensors"] {
            std::fs::copy(model_source.join(file), installed.join(file)).unwrap();
        }
        // Exercise the real production cooldown, without resetting global state
        // or weakening the dependency's registered-artifact admission.
        std::thread::sleep(MODEL_RETRY_COOLDOWN + Duration::from_millis(50));
        assert!(std::ptr::eq(context, get_two_tier_context()));
        assert_eq!(context.availability(), TwoTierAvailability::FastOnly);
        assert!(context.is_available());
        let info = context.fast_info().unwrap();
        assert_eq!(info.dimension, 256);
        assert_eq!(context.config().fast_dimension, info.dimension);
        assert_eq!(info.id, MODEL_POTION_128M);
        let embedding = context
            .embed_fast("mailbox recovery after model installation")
            .unwrap();
        assert_eq!(embedding.len(), info.dimension);
        assert!(embedding.iter().all(|value| value.is_finite()));
        let norm_squared: f32 = embedding.iter().map(|value| value * value).sum();
        assert!((norm_squared - 1.0).abs() < 0.000_01);
        let embedder = get_fast_embedder().unwrap();
        assert!(std::ptr::eq(embedder, get_fast_embedder().unwrap()));
        assert!(std::ptr::eq(info, context.fast_info().unwrap()));
        // Initial metrics remain a historical observation, not current state.
        assert_eq!(
            context.init_metrics().availability,
            TwoTierAvailability::None
        );
        let broken = PathBuf::from(std::env::var_os("FRANKENSEARCH_MODEL_DIR").unwrap());
        for file in ["tokenizer.json", "model.safetensors"] {
            assert_eq!(
                std::fs::read(broken.join(file)).unwrap(),
                b"incomplete installation"
            );
        }
        println!("{VERIFIED}");
    }
}
