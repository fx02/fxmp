use crate::TrackId;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

/// True-random shuffler: each pass is a fresh Fisher-Yates permutation of the
/// pool; no repeats within a pass, and a new pass never starts with the track
/// that was played last.
pub struct Shuffler<R: Rng> {
    pool: Vec<TrackId>,
    /// Remaining tracks of the current pass, consumed from the back.
    order: Vec<TrackId>,
    last: Option<TrackId>,
    first: Option<TrackId>,
    rng: R,
}

impl<R: Rng> Shuffler<R> {
    pub fn new(pool: Vec<TrackId>, avoid_first: Option<TrackId>, rng: R) -> Self {
        let mut s = Shuffler {
            pool,
            order: Vec::new(),
            last: avoid_first,
            first: None,
            rng,
        };
        s.refill();
        s.first = s.order.last().copied();
        s
    }

    fn refill(&mut self) {
        let mut order = self.pool.clone();
        order.shuffle(&mut self.rng);
        // The next track to play is the last element; keep it != avoid.
        let len = order.len();
        if len > 1 && Some(order[len - 1]) == self.last {
            let j = self.rng.gen_range(0..len - 1);
            order.swap(len - 1, j);
        }
        self.order = order;
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<TrackId> {
        if self.pool.is_empty() {
            return None;
        }
        if self.order.is_empty() {
            self.refill();
        }
        let id = self.order.pop()?;
        self.last = Some(id);
        Some(id)
    }

    pub fn remove(&mut self, id: TrackId) {
        self.pool.retain(|&t| t != id);
        self.order.retain(|&t| t != id);
    }

    /// First track of the first pass (to persist in `selection_state`).
    pub fn first(&self) -> Option<TrackId> {
        self.first
    }
}

pub fn entropy_shuffler(pool: Vec<TrackId>, avoid_first: Option<TrackId>) -> Shuffler<StdRng> {
    Shuffler::new(pool, avoid_first, StdRng::from_entropy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn empty_pool_returns_none() {
        let mut s = entropy_shuffler(vec![], None);
        assert_eq!(s.next(), None);
        assert_eq!(s.first(), None);
    }

    #[test]
    fn single_track_pool_repeats_forever() {
        let mut s = entropy_shuffler(vec![7], Some(7));
        for _ in 0..5 {
            assert_eq!(s.next(), Some(7));
        }
    }

    #[test]
    fn no_repeat_within_pass() {
        let mut s = entropy_shuffler((0..100).collect(), None);
        let got: Vec<_> = (0..100).map(|_| s.next().unwrap()).collect();
        let set: HashSet<_> = got.iter().copied().collect();
        assert_eq!(set.len(), 100);
        assert!(got.iter().all(|x| (0..100).contains(x)));
    }

    #[test]
    fn avoid_first_is_respected() {
        for x in 0..10 {
            for _ in 0..50 {
                let s = entropy_shuffler((0..10).collect(), Some(x));
                assert_ne!(s.first(), Some(x));
            }
        }
    }

    #[test]
    fn first_track_distribution_is_uniform() {
        let mut counts = [0u32; 50];
        for _ in 0..1000 {
            let mut s = entropy_shuffler((0..50).collect(), None);
            counts[s.next().unwrap() as usize] += 1;
        }
        assert!(counts.iter().all(|&c| c >= 1), "{counts:?}");
        assert!(counts.iter().all(|&c| c <= 60), "{counts:?}");
    }

    #[test]
    fn new_pass_does_not_start_with_last_played() {
        let mut s = entropy_shuffler((0..5).collect(), None);
        let v: Vec<_> = (0..2000).map(|_| s.next().unwrap()).collect();
        for i in (5..v.len()).step_by(5) {
            assert_ne!(v[i], v[i - 1], "pass boundary at {i}");
        }
    }

    #[test]
    fn remove_skips_removed_track() {
        let mut s = entropy_shuffler((0..10).collect(), None);
        s.remove(3);
        for _ in 0..100 {
            assert_ne!(s.next(), Some(3));
        }
    }
}
