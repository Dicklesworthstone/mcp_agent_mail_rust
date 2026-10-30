//! Bounded outcome evidence without turning forgotten observations into truth.
//!
//! The reservation event ring and its longer-lived index are two views of the
//! same evidence. If pressure discards a newer outcome from the index, an older
//! event still present in the ring must not become the apparent latest outcome.
//! A monotonic timestamp floor excludes that history from both views. Losing
//! evidence therefore makes an experience inconclusive, never successful.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

/// Outcomes at the same timestamp may disagree. Preserve that ambiguity until
/// strictly newer evidence arrives, including after the event ring evicts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OutcomeEvidence<V> {
    pub timestamp_micros: i64,
    pub value: Option<V>,
}

impl<V: Copy + Eq> OutcomeEvidence<V> {
    pub(super) const fn observed(timestamp_micros: i64, value: V) -> Self {
        Self {
            timestamp_micros,
            value: Some(value),
        }
    }

    fn merge(&mut self, incoming: Self) {
        match incoming.timestamp_micros.cmp(&self.timestamp_micros) {
            std::cmp::Ordering::Greater => *self = incoming,
            std::cmp::Ordering::Equal => {
                if self.value != incoming.value {
                    self.value = None;
                }
            }
            std::cmp::Ordering::Less => {}
        }
    }
}

/// Resolve the newest evidence independently of iteration order. An ambiguous
/// newest timestamp must not fall back to an older, apparently certain event.
pub(super) fn latest_outcome<V: Copy + Eq>(
    observations: impl IntoIterator<Item = OutcomeEvidence<V>>,
) -> Option<V> {
    let mut latest: Option<OutcomeEvidence<V>> = None;
    for observation in observations {
        if let Some(previous) = latest.as_mut() {
            previous.merge(observation);
        } else {
            latest = Some(observation);
        }
    }
    latest.and_then(|observation| observation.value)
}

/// At most `capacity` identities, with one ordering entry per retained identity.
///
/// Eviction forgets every outcome through the oldest retained timestamp. This
/// intentionally includes timestamp ties: keeping some tied identities would
/// let an alias or the event ring restore evidence that was just discarded.
/// Clock regressions and old replays cannot move the floor backwards.
#[derive(Debug, Clone)]
pub(super) struct BoundedOutcomes<K, V> {
    capacity: usize,
    entries: HashMap<K, OutcomeEvidence<V>>,
    order: BTreeSet<(i64, K)>,
    forgotten_through: Option<i64>,
}

impl<K: Clone + Eq + Hash + Ord, V: Copy + Eq> BoundedOutcomes<K, V> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: BTreeSet::new(),
            forgotten_through: None,
        }
    }

    /// Whether a timestamp remains usable in either the index or its event ring.
    pub(super) fn retains_timestamp(&self, timestamp_micros: i64) -> bool {
        self.forgotten_through
            .is_none_or(|floor| timestamp_micros > floor)
    }

    pub(super) fn observe(&mut self, key: K, value: V, timestamp_micros: i64) {
        if !self.retains_timestamp(timestamp_micros) {
            return;
        }
        let incoming = OutcomeEvidence::observed(timestamp_micros, value);
        if let Some(previous) = self.entries.get_mut(&key) {
            if timestamp_micros <= previous.timestamp_micros {
                previous.merge(incoming);
                return;
            }
            self.order.remove(&(previous.timestamp_micros, key.clone()));
            *previous = incoming;
        } else {
            self.entries.insert(key.clone(), incoming);
        }
        self.order.insert((timestamp_micros, key));
        if self.entries.len() > self.capacity
            && let Some(&(oldest, _)) = self.order.first()
        {
            self.forget_through(oldest);
        }
    }

    fn forget_through(&mut self, timestamp_micros: i64) {
        let floor = self
            .forgotten_through
            .map_or(timestamp_micros, |previous| previous.max(timestamp_micros));
        self.forgotten_through = Some(floor);
        while self
            .order
            .first()
            .is_some_and(|(timestamp, _)| *timestamp <= floor)
        {
            if let Some((_, key)) = self.order.pop_first() {
                self.entries.remove(&key);
            }
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&K, OutcomeEvidence<V>)> {
        self.entries.iter().map(|(key, evidence)| (key, *evidence))
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        Released,
        Conflicted,
        Renewed,
    }

    fn outcome(cache: &BoundedOutcomes<&str, Outcome>, key: &str) -> Option<Outcome> {
        latest_outcome(
            cache
                .iter()
                .filter_map(|(retained, evidence)| (*retained == key).then_some(evidence)),
        )
    }

    #[test]
    fn newest_observation_survives_old_replays_without_growing_order_index() {
        let mut cache = BoundedOutcomes::new(2);
        for timestamp in 0..1_000 {
            cache.observe("BlueLake", Outcome::Released, timestamp);
            cache.observe("BlueLake", Outcome::Renewed, timestamp - 1);
            assert_eq!(cache.len(), 1);
            assert_eq!(cache.order.len(), 1);
        }
        assert_eq!(outcome(&cache, "BlueLake"), Some(Outcome::Released));
        assert_eq!(cache.forgotten_through, None);
    }

    #[test]
    fn pressure_makes_forgotten_ring_evidence_inconclusive() {
        let mut cache = BoundedOutcomes::new(1);
        let old_ring = OutcomeEvidence::observed(10, Outcome::Released);
        cache.observe("BlueLake", Outcome::Conflicted, 20);
        cache.observe("OtherAgent", Outcome::Renewed, 30);
        assert_eq!(cache.forgotten_through, Some(20));
        let from_ring = std::iter::once(old_ring)
            .filter(|evidence| cache.retains_timestamp(evidence.timestamp_micros));
        let retained = cache
            .iter()
            .filter_map(|(key, evidence)| (*key == "BlueLake").then_some(evidence));
        assert_eq!(latest_outcome(from_ring.chain(retained)), None);

        // Neither the discarded event nor an even older replay may reappear.
        cache.observe("BlueLake", Outcome::Released, 20);
        cache.observe("BlueLake", Outcome::Released, 10);
        assert_eq!(outcome(&cache, "BlueLake"), None);
        assert_eq!(cache.forgotten_through, Some(20));
        cache.observe("BlueLake", Outcome::Released, 40);
        assert_eq!(outcome(&cache, "BlueLake"), Some(Outcome::Released));
    }

    #[test]
    fn pressure_forgets_all_timestamp_ties_and_accepts_only_strictly_newer_evidence() {
        let mut cache = BoundedOutcomes::new(2);
        cache.observe("A", Outcome::Released, 10);
        cache.observe("B", Outcome::Conflicted, 10);
        cache.observe("C", Outcome::Renewed, 10);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.order.len(), 0);
        assert!(!cache.retains_timestamp(10));
        assert!(cache.retains_timestamp(11));
        cache.observe("A", Outcome::Released, 10);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn equal_timestamp_disagreement_is_sticky_until_newer_evidence() {
        let mut cache = BoundedOutcomes::new(2);
        cache.observe("BlueLake", Outcome::Released, 10);
        cache.observe("BlueLake", Outcome::Conflicted, 10);
        cache.observe("BlueLake", Outcome::Released, 10);
        cache.observe("BlueLake", Outcome::Renewed, 9);
        assert_eq!(outcome(&cache, "BlueLake"), None);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.order.len(), 1);
        cache.observe("BlueLake", Outcome::Released, 11);
        assert_eq!(outcome(&cache, "BlueLake"), Some(Outcome::Released));
    }

    #[test]
    fn aliases_with_conflicting_latest_timestamps_are_inconclusive_in_any_order() {
        let released = OutcomeEvidence::observed(20, Outcome::Released);
        let conflicted = OutcomeEvidence::observed(20, Outcome::Conflicted);
        let older = OutcomeEvidence::observed(10, Outcome::Released);
        for evidence in [
            [released, conflicted, older],
            [older, conflicted, released],
            [conflicted, released, older],
        ] {
            assert_eq!(latest_outcome(evidence), None);
        }
        assert_eq!(
            latest_outcome([released, released]),
            Some(Outcome::Released)
        );
    }

    #[test]
    fn zero_capacity_disables_evidence_without_an_unbounded_index() {
        let mut cache = BoundedOutcomes::new(0);
        cache.observe("A", Outcome::Released, 10);
        cache.observe("B", Outcome::Conflicted, 9);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.order.len(), 0);
        assert_eq!(cache.forgotten_through, Some(10));
    }

    #[test]
    fn timestamp_extremes_and_clock_regression_fail_closed() {
        let mut cache = BoundedOutcomes::new(1);
        assert!(cache.retains_timestamp(i64::MIN));
        cache.observe("A", Outcome::Released, i64::MIN);
        cache.observe("B", Outcome::Conflicted, i64::MAX);
        assert_eq!(cache.forgotten_through, Some(i64::MIN));
        cache.observe("C", Outcome::Renewed, i64::MAX);
        assert_eq!(cache.forgotten_through, Some(i64::MAX));
        assert_eq!(cache.len(), 0);
        cache.observe("A", Outcome::Released, 0);
        assert!(!cache.retains_timestamp(i64::MAX));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn bounded_streams_never_disagree_with_complete_evidence() {
        let events = [
            ("A", Outcome::Released, 10),
            ("A", Outcome::Conflicted, 10),
            ("A", Outcome::Renewed, 20),
            ("B", Outcome::Released, 30),
            ("C", Outcome::Conflicted, 40),
            ("A", Outcome::Released, 5),
        ];
        for mut encoding in 0..events.len().pow(5) {
            let mut cache = BoundedOutcomes::new(2);
            let mut complete = Vec::new();
            for _ in 0..5 {
                let (key, value, timestamp) = events[encoding % events.len()];
                encoding /= events.len();
                complete.push((key, OutcomeEvidence::observed(timestamp, value)));
                cache.observe(key, value, timestamp);
                assert!(cache.len() <= 2);
                assert_eq!(cache.len(), cache.order.len());
                for queried in ["A", "B", "C"] {
                    if let Some(retained) = outcome(&cache, queried) {
                        let truth =
                            latest_outcome(complete.iter().filter_map(|(key, evidence)| {
                                (*key == queried).then_some(*evidence)
                            }));
                        assert_eq!(Some(retained), truth, "query {queried}: {complete:?}");
                    }
                }
            }
        }
    }
}
