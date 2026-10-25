//! True-random shuffling: every pass over a selection is a fresh Fisher-Yates
//! permutation from an OS-entropy seeded RNG, with no repeats within a pass. The
//! play queue ([`crate::queue::Queue`]) makes its passes here.

use crate::TrackId;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

/// An RNG seeded from OS entropy: every run of a selection shuffles differently.
pub fn entropy_rng() -> StdRng {
    StdRng::from_entropy()
}

/// One pass over `pool` in play order: a fresh Fisher-Yates permutation whose first
/// track is not `avoid_first` (unless the pool holds nothing else).
pub fn shuffled_pass<R: Rng>(
    pool: &[TrackId],
    avoid_first: Option<TrackId>,
    rng: &mut R,
) -> Vec<TrackId> {
    let mut order = pool.to_vec();
    order.shuffle(rng);
    if order.len() > 1 && Some(order[0]) == avoid_first {
        let j = rng.gen_range(1..order.len());
        order.swap(0, j);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn pass(pool: &[TrackId], avoid: Option<TrackId>) -> Vec<TrackId> {
        shuffled_pass(pool, avoid, &mut entropy_rng())
    }

    #[test]
    fn empty_and_single_track_pools() {
        assert!(pass(&[], Some(1)).is_empty());
        assert_eq!(pass(&[7], Some(7)), [7], "nothing else to start with");
    }

    #[test]
    fn a_pass_has_every_track_once() {
        let pool: Vec<TrackId> = (0..100).collect();
        let got = pass(&pool, None);
        assert_eq!(got.len(), 100);
        assert_eq!(got.iter().copied().collect::<HashSet<_>>().len(), 100);
    }

    #[test]
    fn avoid_first_is_respected() {
        for x in 0..10 {
            for _ in 0..50 {
                assert_ne!(pass(&(0..10).collect::<Vec<_>>(), Some(x))[0], x);
            }
        }
    }

    #[test]
    fn first_track_distribution_is_uniform() {
        let pool: Vec<TrackId> = (0..50).collect();
        let mut counts = [0u32; 50];
        for _ in 0..1000 {
            counts[pass(&pool, None)[0] as usize] += 1;
        }
        assert!(counts.iter().all(|&c| c >= 1), "{counts:?}");
        assert!(counts.iter().all(|&c| c <= 60), "{counts:?}");
    }

    #[test]
    fn entropy_seeding_differs_between_runs() {
        let pool: Vec<TrackId> = (0..1000).collect();
        let runs: HashSet<Vec<TrackId>> = (0..5).map(|_| pass(&pool, None)).collect();
        assert_eq!(runs.len(), 5, "every run shuffles anew");
    }
}
