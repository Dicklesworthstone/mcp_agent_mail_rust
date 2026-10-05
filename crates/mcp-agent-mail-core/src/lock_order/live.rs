//! Live observations, not just statistics for acquisitions that have finished.
//!
//! Each ordered lock owns its metadata independently. The registry is touched
//! only on first use and inspection, never on every acquisition. Registry and
//! metadata locks are private leaves: no protected application value, logging
//! callback, or application lock is accessed while either is held. Inspection
//! uses `try_lock` throughout, so a diagnostic cannot wait behind the stall it is
//! meant to explain. Observations across instances are not an atomic snapshot.

use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, TryLockError, Weak};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{DurationNanosU64 as _, LockLevel};

const MAX_INSTANCES: usize = 4096;
const MAX_PARTICIPANTS: usize = 128;
const MAX_SNAPSHOT_LOCKS: usize = 32;
const MAX_SNAPSHOT_PARTICIPANTS: usize = 64;
const MAX_PARTICIPANTS_PER_PHASE: usize = 8;

/// Kind of access requested or held; shared readers are not exclusive owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockAccess {
    Mutex,
    Read,
    Write,
}

/// One named holder or waiter. Protected values are never inspected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockParticipant {
    pub thread_id: String,
    pub thread_name: Option<String>,
    pub access: LockAccess,
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Time since acquisition for a holder, or since blocking for a waiter.
    pub elapsed_ns: u64,
    pub labels_truncated: bool,
}

/// Observed activity for one instance, not an aggregate of same-ranked shards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockActivity {
    pub instance_id: u64,
    pub lock_name: String,
    pub rank: u16,
    pub holder_count: usize,
    pub waiter_count: usize,
    /// Lower bound when participant tracking or output has been truncated.
    pub oldest_observed_wait_ns: u64,
    pub holders: Vec<LockParticipant>,
    pub waiters: Vec<LockParticipant>,
    pub omitted_participants: usize,
}

/// Bounded, best-effort observation of the instrumented ordered locks only.
///
/// An unavailable/partial observation must not be interpreted as "no blockers".
/// Counts and call sites may change at a handoff; this is not a deadlock proof
/// or an inventory of uninstrumented OS, database-engine, or filesystem locks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockActivitySnapshot {
    pub available: bool,
    pub locks: Vec<LockActivity>,
    pub busy_instances: usize,
    pub omitted_active_instances: usize,
    /// Cumulative first-use registrations refused by the registry bound.
    pub registrations_dropped: u64,
}

impl LockActivitySnapshot {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.available
            && self.busy_instances == 0
            && self.omitted_active_instances == 0
            && self.registrations_dropped == 0
            && self.locks.iter().all(|lock| {
                lock.omitted_participants == 0
                    && lock
                        .holders
                        .iter()
                        .chain(&lock.waiters)
                        .all(|participant| !participant.labels_truncated)
            })
    }
}

#[derive(Debug)]
struct ThreadLabel {
    id: String,
    name: Option<String>,
    truncated: bool,
}

thread_local! {
    // No name formatting/allocation on every acquire. Thread IDs distinguish
    // unnamed workers and workers with identical display names.
    static THREAD_LABEL: Arc<ThreadLabel> = {
        let thread = std::thread::current();
        let name = thread.name().map(|name| bounded_label(name, 64));
        Arc::new(ThreadLabel {
            id: format!("{:?}", thread.id()),
            truncated: name.as_ref().is_some_and(|(_, truncated)| *truncated),
            name: name.map(|(name, _)| name),
        })
    };
}

fn bounded_label(value: &str, max_bytes: usize) -> (String, bool) {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), end < value.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Holding,
    Waiting,
}

#[derive(Debug, Clone)]
struct Participant {
    token: u64,
    thread: Arc<ThreadLabel>,
    access: LockAccess,
    site: &'static Location<'static>,
    since: Instant,
    phase: Phase,
}

impl Participant {
    fn snapshot(&self, now: Instant) -> LockParticipant {
        let (file, truncated) = bounded_label(self.site.file(), 192);
        LockParticipant {
            thread_id: self.thread.id.clone(),
            thread_name: self.thread.name.clone(),
            access: self.access,
            file,
            line: self.site.line(),
            column: self.site.column(),
            elapsed_ns: now.saturating_duration_since(self.since).as_nanos_u64(),
            labels_truncated: truncated || self.thread.truncated,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    next_token: u64,
    holders: usize,
    waiters: usize,
    participants: Vec<Participant>,
}

#[derive(Debug)]
struct Instance {
    id: u64,
    level: LockLevel,
    state: Mutex<State>,
}

struct Registry {
    instances: Mutex<Vec<Weak<Instance>>>,
    next_id: AtomicU64,
    dropped: AtomicU64,
    limit: usize,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry::new(MAX_INSTANCES));

impl Registry {
    const fn new(limit: usize) -> Self {
        Self {
            instances: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
            limit,
        }
    }

    fn register(&self, level: LockLevel) -> Option<Arc<Instance>> {
        let mut instances = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if instances.len() >= self.limit {
            instances.retain(|instance| instance.strong_count() > 0);
        }
        if instances.len() >= self.limit {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let Ok(id) = self
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let instance = Arc::new(Instance {
            id,
            level,
            state: Mutex::new(State::default()),
        });
        instances.push(Arc::downgrade(&instance));
        drop(instances);
        Some(instance)
    }

    fn snapshot(&self) -> LockActivitySnapshot {
        let mut result = LockActivitySnapshot {
            registrations_dropped: self.dropped.load(Ordering::Relaxed),
            ..LockActivitySnapshot::default()
        };
        let instances: Vec<_> = {
            let guard = match self.instances.try_lock() {
                Ok(instances) => instances,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return result,
            };
            guard.iter().filter_map(Weak::upgrade).collect()
        };
        // The registry guard is gone before any per-instance metadata access.
        result.available = true;
        let now = Instant::now();
        let mut active = Vec::new();
        let mut active_count = 0;
        for instance in instances {
            let state = match instance.state.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    result.busy_instances += 1;
                    continue;
                }
            };
            if state.holders == 0 && state.waiters == 0 {
                continue;
            }
            active_count += 1;
            let wait_ns = state
                .participants
                .iter()
                .filter(|participant| participant.phase == Phase::Waiting)
                .map(|participant| {
                    now.saturating_duration_since(participant.since)
                        .as_nanos_u64()
                })
                .max()
                .unwrap_or(0);
            let candidate = Candidate {
                id: instance.id,
                level: instance.level,
                holders: state.holders,
                waiters: state.waiters,
                wait_ns,
                participants: state.participants.clone(),
            };
            drop(state);
            // Keep the most relevant bounded set even if busy cache locks
            // precede a long-stalled lock in the registration order.
            active.push(candidate);
            active.sort_unstable_by_key(Candidate::priority);
            if active.len() > MAX_SNAPSHOT_LOCKS {
                let _ = active.remove(0);
            }
        }
        result.omitted_active_instances = active_count - active.len();
        active.reverse();
        let mut remaining = MAX_SNAPSHOT_PARTICIPANTS;
        for candidate in active {
            result.locks.push(candidate.snapshot(now, &mut remaining));
        }
        result
    }
}

struct Candidate {
    id: u64,
    level: LockLevel,
    holders: usize,
    waiters: usize,
    wait_ns: u64,
    participants: Vec<Participant>,
}

impl Candidate {
    const fn priority(&self) -> (bool, u64, std::cmp::Reverse<u64>) {
        (self.waiters > 0, self.wait_ns, std::cmp::Reverse(self.id))
    }

    fn snapshot(mut self, now: Instant, remaining: &mut usize) -> LockActivity {
        self.participants
            .sort_unstable_by_key(|participant| participant.since);
        let mut lock = LockActivity {
            instance_id: self.id,
            lock_name: format!("{:?}", self.level),
            rank: self.level.rank(),
            holder_count: self.holders,
            waiter_count: self.waiters,
            oldest_observed_wait_ns: self.wait_ns,
            holders: Vec::new(),
            waiters: Vec::new(),
            omitted_participants: self.holders.saturating_add(self.waiters),
        };
        // Reserve room for both sides of a blocked RwLock, rather than
        // allowing a large reader set to hide its waiting writer.
        for phase in [Phase::Holding, Phase::Waiting] {
            let destination = if phase == Phase::Holding {
                &mut lock.holders
            } else {
                &mut lock.waiters
            };
            for participant in self
                .participants
                .iter()
                .filter(|participant| participant.phase == phase)
                .take(MAX_PARTICIPANTS_PER_PHASE.min(*remaining))
            {
                destination.push(participant.snapshot(now));
                *remaining -= 1;
                lock.omitted_participants -= 1;
            }
        }
        lock
    }
}

/// Inspect current holders and blocked acquisitions without taking their locks.
#[must_use]
pub fn lock_activity_snapshot() -> LockActivitySnapshot {
    REGISTRY.snapshot()
}

/// Lazy, per-instance metadata. A cold lock allocates nothing until used.
#[derive(Debug)]
pub(super) struct LiveLock {
    instance: OnceLock<Option<Arc<Instance>>>,
}

impl LiveLock {
    pub(super) const fn new() -> Self {
        Self {
            instance: OnceLock::new(),
        }
    }

    pub(super) fn holding(
        &self,
        level: LockLevel,
        access: LockAccess,
        site: &'static Location<'static>,
        since: Instant,
    ) -> Option<ActivityGuard> {
        self.begin(level, access, site, since, Phase::Holding)
    }

    pub(super) fn waiting(
        &self,
        level: LockLevel,
        access: LockAccess,
        site: &'static Location<'static>,
        since: Instant,
    ) -> Option<ActivityGuard> {
        self.begin(level, access, site, since, Phase::Waiting)
    }

    fn begin(
        &self,
        level: LockLevel,
        access: LockAccess,
        site: &'static Location<'static>,
        since: Instant,
        phase: Phase,
    ) -> Option<ActivityGuard> {
        let instance = self
            .instance
            .get_or_init(|| REGISTRY.register(level))
            .as_ref()?;
        Some(ActivityGuard::begin(instance, access, site, since, phase))
    }
}

/// A unique lease prevents an older guard's drop from clearing a newer owner.
/// It also cleans up a pending wait if acquisition unwinds before completion.
pub(super) struct ActivityGuard {
    instance: Arc<Instance>,
    token: Option<u64>,
    phase: Phase,
}

impl ActivityGuard {
    fn begin(
        instance: &Arc<Instance>,
        access: LockAccess,
        site: &'static Location<'static>,
        since: Instant,
        phase: Phase,
    ) -> Self {
        // A later TLS destructor can still acquire application locks after
        // the cached label has been destroyed. Instrumentation must not add
        // a teardown panic: retain anonymous counts and disclose omissions.
        let thread = THREAD_LABEL.try_with(Arc::clone).ok();
        let mut state = instance
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match phase {
            Phase::Holding => state.holders += 1,
            Phase::Waiting => state.waiters += 1,
        }
        let token = if thread.is_some() && state.participants.len() < MAX_PARTICIPANTS {
            state.next_token.checked_add(1)
        } else {
            None
        };
        if let (Some(token), Some(thread)) = (token, thread) {
            state.next_token = token;
            state.participants.push(Participant {
                token,
                thread,
                access,
                site,
                since,
                phase,
            });
        }
        drop(state);
        Self {
            instance: Arc::clone(instance),
            token,
            phase,
        }
    }

    pub(super) fn acquired(mut self, since: Instant) -> Self {
        let mut state = self
            .instance
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiters = state.waiters.saturating_sub(1);
        state.holders += 1;
        if let Some(participant) = state
            .participants
            .iter_mut()
            .find(|participant| Some(participant.token) == self.token)
        {
            participant.phase = Phase::Holding;
            participant.since = since;
        }
        self.phase = Phase::Holding;
        drop(state);
        self
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut state = self
            .instance
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.phase {
            Phase::Holding => state.holders = state.holders.saturating_sub(1),
            Phase::Waiting => state.waiters = state.waiters.saturating_sub(1),
        }
        if let Some(index) = state
            .participants
            .iter()
            .position(|participant| Some(participant.token) == self.token)
        {
            // Preserve chronological order so bounded observations retain the
            // oldest known waiters/holders rather than random swap order.
            state.participants.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OrderedMutex, OrderedRwLock};
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn instance_id(activity: &LiveLock) -> u64 {
        activity.instance.get().unwrap().as_ref().unwrap().id
    }

    // Poll only diagnostic metadata. Callers always release/join workers
    // before asserting, including when this bounded observation times out.
    fn observe_activity(
        id: u64,
        predicate: impl Fn(&LockActivity) -> bool,
    ) -> Option<LockActivity> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(lock) = lock_activity_snapshot()
                .locks
                .into_iter()
                .find(|lock| lock.instance_id == id && predicate(lock))
            {
                return Some(lock);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn observe_waiter(id: u64) -> Option<LockActivity> {
        observe_activity(id, |lock| lock.waiter_count > 0)
    }

    fn assert_inactive(activity: &LiveLock) {
        let instance = activity.instance.get().unwrap().as_ref().unwrap();
        let state = instance.state.lock().unwrap();
        assert_eq!((state.holders, state.waiters), (0, 0));
        assert!(state.participants.is_empty());
        drop(state);
    }

    #[test]
    fn real_mutex_exposes_unfinished_wait_and_exact_acquisition_sites() {
        let mutex = Arc::new(OrderedMutex::new(
            LockLevel::StorageCommitQueue,
            "private mailbox value",
        ));
        let owner_line = line!() + 1;
        let owner = mutex.lock();
        let id = instance_id(&mutex.activity);
        let (site_tx, site_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let peer = Arc::clone(&mutex);
        let worker = std::thread::Builder::new()
            .name("blocked-archive-worker".into())
            .spawn(move || {
                site_tx.send(line!() + 1).unwrap();
                let guard = peer.lock();
                acquired_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
                assert_eq!(*guard, "private mailbox value");
            })
            .unwrap();
        let waiter_line = site_rx.recv_timeout(Duration::from_secs(5));
        let before = observe_waiter(id);
        drop(owner);
        let acquired = acquired_rx.recv_timeout(Duration::from_secs(5));
        let after = observe_activity(id, |lock| lock.holder_count == 1 && lock.waiter_count == 0);
        let _ = release_tx.send(());
        worker.join().unwrap();

        let before = before.expect("blocked acquisition must be visible before it finishes");
        assert_eq!((before.holder_count, before.waiter_count), (1, 1));
        assert_eq!(before.holders[0].line, owner_line);
        assert_eq!(before.waiters[0].line, waiter_line.unwrap());
        assert_eq!(before.waiters[0].file, file!());
        assert_eq!(
            before.waiters[0].thread_name.as_deref(),
            Some("blocked-archive-worker")
        );
        assert_ne!(before.holders[0].thread_id, before.waiters[0].thread_id);
        assert!(before.oldest_observed_wait_ns > 0);
        assert!(
            !serde_json::to_string(&before)
                .unwrap()
                .contains("private mailbox value")
        );
        acquired.unwrap();
        let after = after.expect("acquired waiter must replace, not duplicate, the old holder");
        assert_eq!((after.holder_count, after.waiter_count), (1, 0));
        assert_eq!(after.holders[0].thread_id, before.waiters[0].thread_id);
        assert_inactive(&mutex.activity);
    }

    #[test]
    fn real_rwlock_names_both_readers_blocking_a_writer() {
        let lock = Arc::new(OrderedRwLock::new(LockLevel::DbReadCacheProjectsBySlug, 0));
        let first = lock.read();
        let id = instance_id(&lock.activity);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let peer = Arc::clone(&lock);
        let reader = std::thread::spawn(move || {
            let guard = peer.read();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            drop(guard);
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(5));
        let peer = Arc::clone(&lock);
        let writer = std::thread::spawn(move || *peer.write() = 42);
        let snapshot = observe_waiter(id);
        let _ = release_tx.send(());
        drop(first);
        reader.join().unwrap();
        writer.join().unwrap();
        ready.unwrap();
        let snapshot = snapshot.unwrap();
        assert_eq!((snapshot.holder_count, snapshot.waiter_count), (2, 1));
        assert_eq!(snapshot.holders.len(), 2);
        assert!(
            snapshot
                .holders
                .iter()
                .all(|holder| holder.access == LockAccess::Read)
        );
        assert_eq!(snapshot.waiters[0].access, LockAccess::Write);
        assert_ne!(snapshot.holders[0].thread_id, snapshot.holders[1].thread_id);
        assert_eq!(*lock.read(), 42);
        assert_inactive(&lock.activity);
    }

    #[test]
    fn real_rwlock_exposes_reader_waiting_for_a_writer() {
        let lock = Arc::new(OrderedRwLock::new(LockLevel::StorageRepoCache, 42));
        let owner = lock.write();
        let id = instance_id(&lock.activity);
        let peer = Arc::clone(&lock);
        let reader = std::thread::spawn(move || *peer.read());
        let snapshot = observe_waiter(id);
        drop(owner);
        assert_eq!(reader.join().unwrap(), 42);
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.holders[0].access, LockAccess::Write);
        assert_eq!(snapshot.waiters[0].access, LockAccess::Read);
    }

    #[test]
    fn failed_try_lock_does_not_register_a_blocked_waiter() {
        let lock = Arc::new(OrderedMutex::new(LockLevel::StorageWbqStats, ()));
        let owner = lock.lock();
        let id = instance_id(&lock.activity);
        let peer = Arc::clone(&lock);
        let failed = std::thread::spawn(move || peer.try_lock().is_none())
            .join()
            .unwrap();
        let snapshot = observe_activity(id, |lock| lock.holder_count == 1);
        drop(owner);
        assert!(failed);
        let instance = snapshot.unwrap();
        assert_eq!((instance.holder_count, instance.waiter_count), (1, 0));
        assert_eq!(instance.waiters, [] as [LockParticipant; 0]);
        let owner = lock.try_lock().expect("successful try_lock is tracked");
        let snapshot = observe_activity(id, |lock| lock.holder_count == 1);
        drop(owner);
        assert_eq!(snapshot.unwrap().holder_count, 1);
        assert_inactive(&lock.activity);
    }

    #[test]
    // The fixture thread must panic while its guard is held, or nothing is
    // poisoned.
    #[allow(clippy::significant_drop_tightening)]
    fn poisoned_mutex_recovery_and_unwind_leave_no_phantom_owners() {
        let lock = Arc::new(OrderedMutex::new(LockLevel::DbSqliteInitGates, 0));
        let peer = Arc::clone(&lock);
        assert!(
            std::thread::spawn(move || {
                let mut guard = peer.lock();
                *guard = 7;
                panic!("fixture: poison the application mutex, not its metadata");
            })
            .join()
            .is_err()
        );
        let id = instance_id(&lock.activity);
        assert_inactive(&lock.activity);
        let recovered = lock.lock();
        let snapshot = observe_activity(id, |lock| lock.holder_count == 1);
        assert_eq!(*recovered, 7);
        drop(recovered);
        assert_eq!(snapshot.unwrap().holder_count, 1);
        assert_inactive(&lock.activity);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn inversion_names_both_real_call_sites_and_does_not_publish_a_wait() {
        let high = OrderedMutex::new(LockLevel::ToolsToolMetrics, ());
        let low = OrderedRwLock::new(LockLevel::DbPoolCache, ());
        let held_line = line!() + 1;
        let held = high.lock();
        let attempted_line = line!() + 2;
        let failure = std::panic::catch_unwind(|| {
            drop(low.write());
        });
        drop(held);
        let failure = failure.expect_err("rank inversion must be refused before blocking");
        let text = failure.downcast_ref::<String>().expect("formatted panic");
        assert!(text.contains("lock order violation"));
        assert!(text.contains(&format!("{}:{held_line}:", file!())));
        assert!(text.contains(&format!("{}:{attempted_line}:", file!())));
        assert!(low.activity.instance.get().is_none());
        // The caught refusal did not corrupt this thread's ordering stack.
        drop(low.write());
    }

    #[test]
    fn weak_registry_reclaims_slots_and_reports_refused_registrations() {
        let registry = Registry::new(1);
        let first = registry.register(LockLevel::DbPoolCache).unwrap();
        assert!(registry.register(LockLevel::StorageCommitQueue).is_none());
        assert_eq!(registry.snapshot().registrations_dropped, 1);
        let old_id = first.id;
        drop(first);
        let second = registry.register(LockLevel::StorageCommitQueue).unwrap();
        assert_ne!(second.id, old_id);
        assert_eq!(registry.instances.lock().unwrap().len(), 1);
    }

    #[test]
    fn metadata_and_registry_contention_are_partial_not_empty_successes() {
        let registry = Registry::new(2);
        let instance = registry.register(LockLevel::DbPoolCache).unwrap();
        let owner = ActivityGuard::begin(
            &instance,
            LockAccess::Mutex,
            Location::caller(),
            Instant::now(),
            Phase::Holding,
        );
        let held = instance.state.lock().unwrap();
        let snapshot = registry.snapshot();
        drop(held);
        assert!(snapshot.available);
        assert_eq!(snapshot.busy_instances, 1);
        assert!(!snapshot.is_complete());
        let held = registry.instances.lock().unwrap();
        let snapshot = registry.snapshot();
        drop(held);
        assert!(!snapshot.available);
        assert!(!snapshot.is_complete());
        drop(owner);
        assert!(registry.snapshot().is_complete());
    }

    #[test]
    fn participant_bound_keeps_counts_and_cleans_up_unrecorded_guards() {
        let registry = Registry::new(1);
        let instance = registry.register(LockLevel::DbPoolCache).unwrap();
        let guards: Vec<_> = (0..MAX_PARTICIPANTS + 3)
            .map(|_| {
                ActivityGuard::begin(
                    &instance,
                    LockAccess::Read,
                    Location::caller(),
                    Instant::now(),
                    Phase::Holding,
                )
            })
            .collect();
        assert_eq!(
            instance.state.lock().unwrap().participants.len(),
            MAX_PARTICIPANTS
        );
        let snapshot = registry.snapshot();
        let lock = &snapshot.locks[0];
        assert_eq!(lock.holder_count, MAX_PARTICIPANTS + 3);
        assert_eq!(lock.holders.len(), MAX_PARTICIPANTS_PER_PHASE);
        assert_eq!(
            lock.omitted_participants,
            lock.holder_count - lock.holders.len()
        );
        assert!(!snapshot.is_complete());
        drop(guards);
        assert_eq!(registry.snapshot().locks, [] as [LockActivity; 0]);
        assert!(registry.snapshot().is_complete());
    }

    #[test]
    fn wait_to_hold_transition_is_one_identity_and_drop_removes_only_it() {
        let registry = Registry::new(1);
        let instance = registry.register(LockLevel::StorageCommitQueue).unwrap();
        let owner = ActivityGuard::begin(
            &instance,
            LockAccess::Mutex,
            Location::caller(),
            Instant::now(),
            Phase::Holding,
        );
        let waiting = ActivityGuard::begin(
            &instance,
            LockAccess::Mutex,
            Location::caller(),
            Instant::now(),
            Phase::Waiting,
        );
        let before = registry.snapshot();
        assert_eq!(
            (before.locks[0].holder_count, before.locks[0].waiter_count),
            (1, 1)
        );
        let next_owner = waiting.acquired(Instant::now());
        drop(owner);
        let after = registry.snapshot();
        assert_eq!(
            (after.locks[0].holder_count, after.locks[0].waiter_count),
            (1, 0)
        );
        drop(next_owner);
        assert_eq!(registry.snapshot().locks, [] as [LockActivity; 0]);
    }

    #[test]
    fn snapshot_bounds_prioritize_blocked_instances_over_idle_holders() {
        let registry = Registry::new(MAX_SNAPSHOT_LOCKS + 2);
        let mut instances = Vec::new();
        let mut guards = Vec::new();
        for _ in 0..=MAX_SNAPSHOT_LOCKS {
            let instance = registry.register(LockLevel::DbPoolCache).unwrap();
            guards.push(ActivityGuard::begin(
                &instance,
                LockAccess::Mutex,
                Location::caller(),
                Instant::now(),
                Phase::Holding,
            ));
            instances.push(instance);
        }
        let stalled = registry.register(LockLevel::StorageCommitQueue).unwrap();
        let waiter = ActivityGuard::begin(
            &stalled,
            LockAccess::Mutex,
            Location::caller(),
            Instant::now(),
            Phase::Waiting,
        );
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.locks.len(), MAX_SNAPSHOT_LOCKS);
        assert_eq!(snapshot.locks[0].instance_id, stalled.id);
        assert_eq!(snapshot.omitted_active_instances, 2);
        assert!(
            snapshot
                .locks
                .iter()
                .map(|lock| lock.holders.len() + lock.waiters.len())
                .sum::<usize>()
                <= MAX_SNAPSHOT_PARTICIPANTS
        );
        drop(waiter);
        drop(guards);
        drop(instances);
    }

    #[test]
    fn labels_truncate_only_at_utf8_boundaries() {
        let (text, truncated) = bounded_label("ééé", 5);
        assert_eq!(text, "éé");
        assert!(truncated);
        assert_eq!(bounded_label("ok", 64), ("ok".to_string(), false));
        assert_eq!(bounded_label("é", 0), (String::new(), true));
    }

    #[cfg(unix)]
    #[test]
    fn destroyed_thread_label_retains_anonymous_counts_without_panicking() {
        struct LateUse {
            instance: Arc<Instance>,
            result: mpsc::Sender<(bool, bool)>,
        }
        impl Drop for LateUse {
            fn drop(&mut self) {
                let label_gone = THREAD_LABEL.try_with(|_| ()).is_err();
                let instance = Arc::clone(&self.instance);
                // Catch the old with() panic inside the destructor, so a
                // regression fails this test instead of aborting the suite.
                let outcome = std::panic::catch_unwind(move || {
                    let guard = ActivityGuard::begin(
                        &instance,
                        LockAccess::Mutex,
                        Location::caller(),
                        Instant::now(),
                        Phase::Holding,
                    );
                    let state = instance.state.lock().unwrap();
                    let anonymous = state.holders == 1 && state.participants.is_empty();
                    drop(state);
                    drop(guard);
                    anonymous
                });
                let _ = self.result.send((label_gone, outcome.unwrap_or(false)));
            }
        }
        thread_local! {
            static LATE_USE: std::cell::RefCell<Option<LateUse>> = const {
                std::cell::RefCell::new(None)
            };
        }
        let registry = Registry::new(1);
        let instance = registry.register(LockLevel::StorageCommitQueue).unwrap();
        let retained = Arc::clone(&instance);
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Initialize this destructor before the label cache, so it runs
            // after that cache has been destroyed at this real thread's exit.
            LATE_USE.with(|slot| {
                *slot.borrow_mut() = Some(LateUse {
                    instance: Arc::clone(&retained),
                    result: result_tx,
                });
            });
            drop(ActivityGuard::begin(
                &retained,
                LockAccess::Mutex,
                Location::caller(),
                Instant::now(),
                Phase::Holding,
            ));
        })
        .join()
        .unwrap();
        let (label_gone, anonymous) = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            label_gone,
            "exercise a destroyed TLS label, not the ordinary cache path"
        );
        assert!(
            anonymous,
            "missing labels must not prevent lock-activity cleanup"
        );
        let state = instance.state.lock().unwrap();
        assert_eq!((state.holders, state.waiters), (0, 0));
        assert!(state.participants.is_empty());
        drop(state);
    }
}
