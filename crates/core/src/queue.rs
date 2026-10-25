//! The play queue ("Playlist"): the tracks of a selection in play order, the
//! position in it, manual edits, and a history for going back.
//!
//! The queue only decides; the player asks it for the track that is due
//! ([`Queue::successor`]) and tells it when a track really started
//! ([`Queue::started`]). Asking twice without a start gives the same answer, so a
//! prepared track can be dropped and asked for again after the queue changed.
//!
//! - **Order** – the list shown as the playlist. For a shuffle it is the current
//!   pass (a permutation of the pool, plus manual entries); when it runs out, the next
//!   pass is a fresh shuffle of its non-manual entries that does not start with the
//!   track played last. A fixed list (an album, a hand-made list) just ends.
//! - **Cursor** – the index of the last track started from the order; what follows
//!   it is upcoming.
//! - **History** – the last [`HISTORY_LEN`] tracks played before the current one.
//!   Going back walks it without touching the order; going forward again replays it
//!   before the order continues, so a shuffle's order stays as it was.

use std::collections::VecDeque;

use rand::Rng;

use crate::shuffle::shuffled_pass;
use crate::TrackId;

/// How many played tracks can be gone back to.
pub const HISTORY_LEN: usize = 100;

/// A track in the order; `manual` when it was added by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub id: TrackId,
    pub manual: bool,
}

impl Entry {
    pub fn new(id: TrackId) -> Self {
        Self { id, manual: false }
    }
    pub fn manual(id: TrackId) -> Self {
        Self { id, manual: true }
    }
}

/// What the queue was asked to play next, ahead of the normal order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    /// The current track again, from the start (or from where it was, on a resume).
    Restart,
    /// The track before the current one in the history.
    Back,
}

/// Everything needed to rebuild a queue (see [`Queue::state`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueueState {
    pub shuffle: bool,
    pub order: Vec<Entry>,
    pub cursor: Option<usize>,
    /// Played tracks, oldest first, the current one included.
    pub history: Vec<TrackId>,
    /// Index of the current track in `history`.
    pub history_pos: Option<usize>,
}

pub struct Queue<R: Rng> {
    shuffle: bool,
    order: Vec<Entry>,
    cursor: Option<usize>,
    /// The next shuffle pass, made when the current one runs out and swapped in
    /// when its first track starts.
    next_pass: Option<Vec<Entry>>,
    history: VecDeque<TrackId>,
    history_pos: Option<usize>,
    target: Option<Target>,
    rng: R,
    /// Bumped whenever the order changes.
    version: u64,
}

impl<R: Rng> Queue<R> {
    /// A shuffle of `pool` whose first track is not `avoid_first`.
    pub fn shuffled(pool: &[TrackId], avoid_first: Option<TrackId>, mut rng: R) -> Self {
        let order = shuffled_pass(pool, avoid_first, &mut rng)
            .into_iter()
            .map(Entry::new)
            .collect();
        Self::with_order(true, order, rng)
    }

    /// `ids` in this order, once (an album, a hand-made list; empty for a new one).
    pub fn fixed(ids: &[TrackId], rng: R) -> Self {
        Self::with_order(false, ids.iter().copied().map(Entry::new).collect(), rng)
    }

    fn with_order(shuffle: bool, order: Vec<Entry>, rng: R) -> Self {
        Self {
            shuffle,
            order,
            cursor: None,
            next_pass: None,
            history: VecDeque::new(),
            history_pos: None,
            target: None,
            rng,
            version: 0,
        }
    }

    /// Rebuilds a saved queue. With `resume`, the current track is due again (to
    /// continue where it stopped); otherwise the order continues after it.
    pub fn restore(state: QueueState, resume: bool, rng: R) -> Self {
        let len = state.order.len();
        let history: VecDeque<TrackId> = state.history.into_iter().collect();
        let history_pos = state.history_pos.filter(|&p| p < history.len());
        let mut q = Self::with_order(state.shuffle, state.order, rng);
        q.cursor = state.cursor.filter(|&c| c < len);
        q.history = history;
        q.history_pos = history_pos;
        if resume && history_pos.is_some() {
            q.target = Some(Target::Restart);
        }
        q
    }

    pub fn state(&self) -> QueueState {
        QueueState {
            shuffle: self.shuffle,
            order: self.order.clone(),
            cursor: self.cursor,
            history: self.history.iter().copied().collect(),
            history_pos: self.history_pos,
        }
    }

    pub fn is_shuffle(&self) -> bool {
        self.shuffle
    }

    /// Changes whenever the order does.
    pub fn version(&self) -> u64 {
        self.version
    }

    fn changed(&mut self) {
        self.version += 1;
    }

    pub fn order(&self) -> &[Entry] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Index of the last track started from the order.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// The playing (or last played) track.
    pub fn current(&self) -> Option<TrackId> {
        self.history_pos.map(|p| self.history[p])
    }

    /// Index in the order of the current track, if it is the one at the cursor (not
    /// one gone back to or played from elsewhere).
    pub fn current_index(&self) -> Option<usize> {
        let c = self.cursor?;
        (Some(self.order[c].id) == self.current()).then_some(c)
    }

    /// The upcoming entries: the ones after the cursor.
    pub fn upcoming(&self) -> &[Entry] {
        &self.order[self.cursor.map_or(0, |c| c + 1)..]
    }

    /// The track due next. Asking again without a [`Self::started`] in between gives
    /// the same track (unless the queue was changed meanwhile).
    pub fn successor(&mut self) -> Option<TrackId> {
        match self.target {
            Some(Target::Restart) => return self.current(),
            Some(Target::Back) => {
                if let Some(p) = self.history_pos.filter(|&p| p > 0) {
                    return Some(self.history[p - 1]);
                }
            }
            None => {}
        }
        if let Some(p) = self.history_pos.filter(|&p| p + 1 < self.history.len()) {
            return Some(self.history[p + 1]);
        }
        let next = self.cursor.map_or(0, |c| c + 1);
        if let Some(e) = self.order.get(next) {
            return Some(e.id);
        }
        if !self.shuffle {
            return None;
        }
        if self.next_pass.is_none() {
            let pool: Vec<TrackId> = self
                .order
                .iter()
                .filter(|e| !e.manual)
                .map(|e| e.id)
                .collect();
            let pass = shuffled_pass(&pool, self.current(), &mut self.rng);
            self.next_pass = Some(pass.into_iter().map(Entry::new).collect());
        }
        self.next_pass.as_ref()?.first().map(|e| e.id)
    }

    /// `id` started playing. False when the queue did not expect it (a track asked
    /// for just before the queue changed): it only goes into the history.
    pub fn started(&mut self, id: TrackId) -> bool {
        match self.target.take() {
            Some(Target::Restart) if self.current() == Some(id) => return true,
            Some(Target::Back) => {
                if let Some(p) = self.history_pos.filter(|&p| p > 0) {
                    if self.history[p - 1] == id {
                        self.history_pos = Some(p - 1);
                        return true;
                    }
                }
            }
            other => self.target = other,
        }
        if let Some(p) = self.history_pos.filter(|&p| p + 1 < self.history.len()) {
            if self.history[p + 1] == id {
                self.history_pos = Some(p + 1);
                return true;
            }
        }
        let next = self.cursor.map_or(0, |c| c + 1);
        let expected = if self.order.get(next).is_some_and(|e| e.id == id) {
            self.cursor = Some(next);
            true
        } else if self
            .next_pass
            .as_ref()
            .and_then(|p| p.first())
            .is_some_and(|e| e.id == id)
        {
            self.order = self.next_pass.take().unwrap_or_default();
            self.cursor = Some(0);
            self.changed();
            true
        } else {
            false
        };
        self.push_history(id);
        expected
    }

    fn push_history(&mut self, id: TrackId) {
        if let Some(p) = self.history_pos {
            self.history.truncate(p + 1);
        } else {
            self.history.clear();
        }
        self.history.push_back(id);
        while self.history.len() > HISTORY_LEN + 1 {
            self.history.pop_front();
        }
        self.history_pos = Some(self.history.len() - 1);
    }

    /// Drops the tracks gone back from (they are not replayed going forward).
    fn drop_forward_history(&mut self) {
        if let Some(p) = self.history_pos {
            self.history.truncate(p + 1);
        }
    }

    /// Whether there is a played track to go back to.
    pub fn can_go_back(&self) -> bool {
        self.history_pos.is_some_and(|p| p > 0)
    }

    /// The track before the current one is due next. False (and nothing changes)
    /// when there is none.
    pub fn go_back(&mut self) -> bool {
        if !self.can_go_back() {
            return false;
        }
        self.target = Some(Target::Back);
        true
    }

    /// The current track is due again, from the start.
    pub fn restart(&mut self) -> bool {
        if self.current().is_none() {
            return false;
        }
        self.target = Some(Target::Restart);
        true
    }

    /// The entry at `index` is due next, and the order continues after it. Returns
    /// its track, or `None` if there is no such entry.
    pub fn jump(&mut self, index: usize) -> Option<TrackId> {
        let id = self.order.get(index)?.id;
        self.target = None;
        self.drop_forward_history();
        self.cursor = index.checked_sub(1);
        self.next_pass = None;
        Some(id)
    }

    /// Puts `ids` (manual entries) right after the current track: they play next.
    pub fn insert_next(&mut self, ids: &[TrackId]) {
        if ids.is_empty() {
            return;
        }
        let at = self.cursor.map_or(0, |c| c + 1);
        self.order
            .splice(at..at, ids.iter().copied().map(Entry::manual));
        if self.target == Some(Target::Back) {
            self.target = None;
        }
        self.drop_forward_history();
        self.changed();
    }

    /// Adds `ids` (manual entries) at the end of the order.
    pub fn append(&mut self, ids: &[TrackId]) {
        if ids.is_empty() {
            return;
        }
        self.order.extend(ids.iter().copied().map(Entry::manual));
        self.next_pass = None;
        self.changed();
    }

    /// Takes the entry at `index` out of the order (a shuffle's next passes too).
    /// The track keeps playing if it is the current one.
    pub fn remove(&mut self, index: usize) -> Option<Entry> {
        if index >= self.order.len() {
            return None;
        }
        let e = self.order.remove(index);
        if let Some(c) = self.cursor.filter(|&c| index <= c) {
            self.cursor = c.checked_sub(1);
        }
        self.next_pass = None;
        self.changed();
        Some(e)
    }

    /// Moves the entry at `from` to `to` (its index afterwards).
    pub fn move_entry(&mut self, from: usize, to: usize) -> bool {
        let len = self.order.len();
        if from >= len || to >= len {
            return false;
        }
        if from == to {
            return true;
        }
        let e = self.order.remove(from);
        self.order.insert(to, e);
        if let Some(c) = self.cursor {
            self.cursor = Some(if c == from {
                to
            } else if from < c && to >= c {
                c - 1
            } else if from > c && to <= c {
                c + 1
            } else {
                c
            });
        }
        self.changed();
        true
    }

    /// `id` is gone for good (banned, deleted, failed): it leaves the order, the
    /// next pass and the history.
    pub fn forget(&mut self, id: TrackId) {
        self.forget_where(|t| t == id);
    }

    /// [`Self::forget`] for every track `gone` says. The current track stays the
    /// current one (it may still be playing), but is not played again from here.
    pub fn forget_where(&mut self, gone: impl Fn(TrackId) -> bool) {
        let before = self.order.len();
        let mut kept = 0;
        let mut cursor = None;
        for (i, e) in std::mem::take(&mut self.order).into_iter().enumerate() {
            if self.cursor.is_some_and(|c| i <= c) && !gone(e.id) {
                cursor = Some(kept);
            }
            if !gone(e.id) {
                self.order.push(e);
                kept += 1;
            }
        }
        if self.cursor.is_some() {
            self.cursor = cursor;
        }
        if let Some(pass) = self.next_pass.as_mut() {
            pass.retain(|e| !gone(e.id));
        }
        if self.current().is_some_and(&gone) && self.target == Some(Target::Restart) {
            self.target = None;
        }
        let mut i = 0;
        while i < self.history.len() {
            if gone(self.history[i]) && Some(i) != self.history_pos {
                self.history.remove(i);
                if let Some(p) = self.history_pos.filter(|&p| i < p) {
                    self.history_pos = Some(p - 1);
                }
            } else {
                i += 1;
            }
        }
        if self.target == Some(Target::Back) && !self.can_go_back() {
            self.target = None;
        }
        if self.order.len() != before {
            self.changed();
        }
    }

    /// `id` could not be played this time (network trouble): it stays in the list,
    /// but the queue moves past it.
    pub fn pass_over(&mut self, id: TrackId) {
        match self.target {
            Some(Target::Restart) if self.current() == Some(id) => {
                self.target = None;
                return;
            }
            Some(Target::Back) => {
                if let Some(p) = self.history_pos.filter(|&p| p > 0) {
                    if self.history[p - 1] == id {
                        self.target = None;
                        return;
                    }
                }
            }
            _ => {}
        }
        if let Some(p) = self.history_pos.filter(|&p| p + 1 < self.history.len()) {
            if self.history[p + 1] == id {
                self.history.remove(p + 1);
                return;
            }
        }
        let next = self.cursor.map_or(0, |c| c + 1);
        if self.order.get(next).is_some_and(|e| e.id == id) {
            self.cursor = Some(next);
        } else if self
            .next_pass
            .as_ref()
            .and_then(|p| p.first())
            .is_some_and(|e| e.id == id)
        {
            self.order = self.next_pass.take().unwrap_or_default();
            self.cursor = Some(0);
            self.changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    fn rng() -> StdRng {
        StdRng::seed_from_u64(42)
    }

    /// Plays the next track the way the player does: ask, then start.
    fn play(q: &mut Queue<StdRng>) -> Option<TrackId> {
        let id = q.successor()?;
        assert_eq!(q.successor(), Some(id), "asking twice gives the same track");
        q.started(id);
        Some(id)
    }

    fn ids(entries: &[Entry]) -> Vec<TrackId> {
        entries.iter().map(|e| e.id).collect()
    }

    #[test]
    fn a_fixed_list_plays_once_in_order() {
        let mut q = Queue::fixed(&[3, 1, 2], rng());
        let got: Vec<_> = std::iter::from_fn(|| play(&mut q)).collect();
        assert_eq!(got, [3, 1, 2]);
        assert_eq!(q.current(), Some(2));
    }

    #[test]
    fn upcoming_entries_are_what_plays_next() {
        let pool: Vec<TrackId> = (1..=50).collect();
        let mut q = Queue::shuffled(&pool, None, rng());
        play(&mut q);
        play(&mut q);
        let page: Vec<TrackId> = ids(&q.upcoming()[..20]);
        let next: Vec<TrackId> = (0..20).map(|_| play(&mut q).unwrap()).collect();
        assert_eq!(next, page);
    }

    #[test]
    fn a_shuffle_pass_has_no_repeats_and_the_next_does_not_start_with_the_last() {
        for seed in 0..50 {
            let mut q = Queue::shuffled(&[1, 2, 3, 4], Some(1), StdRng::seed_from_u64(seed));
            let first: Vec<_> = (0..4).map(|_| play(&mut q).unwrap()).collect();
            assert_ne!(first[0], 1, "avoids the previous first track");
            assert_eq!(first.iter().copied().collect::<HashSet<_>>().len(), 4);
            // The list shows the old pass until the new one starts.
            assert_eq!(ids(q.order()), first);
            let next = play(&mut q).unwrap();
            assert_ne!(next, first[3], "new pass starts with the last played");
            assert_eq!(q.cursor(), Some(0));
            assert_eq!(q.order()[0].id, next);
        }
    }

    #[test]
    fn back_and_forward_walk_the_history_and_keep_the_order() {
        let pool: Vec<TrackId> = (1..=30).collect();
        let mut q = Queue::shuffled(&pool, None, rng());
        let order = ids(q.order());
        let a = play(&mut q).unwrap();
        let b = play(&mut q).unwrap();
        let c = play(&mut q).unwrap();
        assert!(q.go_back());
        assert_eq!(play(&mut q), Some(b));
        assert!(q.go_back());
        assert_eq!(play(&mut q), Some(a));
        assert!(!q.go_back(), "nothing before the first track");
        assert_eq!(q.current(), Some(a));
        // Forward again: the history first, then the order where it was.
        assert_eq!(play(&mut q), Some(b));
        assert_eq!(play(&mut q), Some(c));
        let rest: Vec<_> = (0..5).map(|_| play(&mut q).unwrap()).collect();
        assert_eq!(rest, order[3..8]);
        assert_eq!(ids(q.order()), order, "order untouched");
    }

    #[test]
    fn going_back_works_in_a_fixed_list_too() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        play(&mut q);
        q.go_back();
        assert_eq!(play(&mut q), Some(1));
        assert_eq!(play(&mut q), Some(2));
        assert_eq!(play(&mut q), Some(3));
        assert_eq!(play(&mut q), None);
    }

    #[test]
    fn the_history_keeps_the_last_100_tracks() {
        let pool: Vec<TrackId> = (1..=500).collect();
        let mut q = Queue::fixed(&pool, rng());
        for _ in 0..150 {
            play(&mut q);
        }
        let mut back = 0;
        while q.go_back() {
            play(&mut q);
            back += 1;
        }
        assert_eq!(back, HISTORY_LEN);
        assert_eq!(q.current(), Some(50));
    }

    #[test]
    fn restart_plays_the_current_track_again_without_history() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        play(&mut q);
        assert!(q.restart());
        assert_eq!(play(&mut q), Some(2));
        q.go_back();
        assert_eq!(play(&mut q), Some(1), "the restart left no history entry");
        assert_eq!(play(&mut q), Some(2));
        assert_eq!(play(&mut q), Some(3));
    }

    #[test]
    fn jumping_keeps_the_order_and_continues_from_there() {
        let pool: Vec<TrackId> = (1..=40).collect();
        let mut q = Queue::shuffled(&pool, None, rng());
        let order = ids(q.order());
        let first = play(&mut q).unwrap();
        assert_eq!(q.jump(10), Some(order[10]));
        assert_eq!(play(&mut q), Some(order[10]));
        assert_eq!(q.cursor(), Some(10));
        assert_eq!(q.current_index(), Some(10));
        assert_eq!(play(&mut q), Some(order[11]));
        assert_eq!(ids(q.order()), order);
        // Back goes to the track played before the jump.
        q.go_back();
        q.go_back();
        assert_eq!(q.successor(), Some(order[10]));
        play(&mut q);
        q.go_back();
        assert_eq!(play(&mut q), Some(first));
        assert_eq!(q.jump(99), None);
    }

    #[test]
    fn a_start_the_queue_did_not_expect_leaves_the_order_alone() {
        let mut q = Queue::fixed(&[1, 2, 3, 4], rng());
        play(&mut q);
        // The player prepared 2, then the user jumped to 4; 2 started before the
        // jump reached the player.
        q.jump(3);
        q.started(2);
        assert_eq!(q.current(), Some(2));
        assert_eq!(q.successor(), Some(4));
    }

    #[test]
    fn started_says_whether_the_queue_expected_that_track() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        assert!(q.started(1), "the first track");
        assert!(
            !q.started(9),
            "a track from elsewhere (a race with a new selection)"
        );
        q.go_back();
        assert!(q.started(1), "going back");
        assert!(q.started(9), "forward through the history");
        q.restart();
        assert!(q.started(9), "a restart");
    }

    #[test]
    fn manual_entries_play_next_or_at_the_end() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        q.insert_next(&[7, 8]);
        q.append(&[9]);
        let rest: Vec<_> = std::iter::from_fn(|| play(&mut q)).collect();
        assert_eq!(rest, [7, 8, 2, 3, 9]);
        assert_eq!(
            q.order().iter().filter(|e| e.manual).count(),
            3,
            "marked as manual"
        );
    }

    #[test]
    fn play_next_comes_before_the_tracks_gone_back_from() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        play(&mut q);
        q.go_back();
        play(&mut q);
        q.insert_next(&[9]);
        assert_eq!(play(&mut q), Some(9));
        assert_eq!(
            play(&mut q),
            Some(3),
            "2 was played: the order goes on after it"
        );
    }

    #[test]
    fn a_new_empty_list_fills_by_hand() {
        let mut q = Queue::fixed(&[], rng());
        assert_eq!(q.successor(), None);
        q.append(&[5]);
        q.insert_next(&[4]);
        assert_eq!(play(&mut q), Some(4));
        assert_eq!(play(&mut q), Some(5));
        assert_eq!(play(&mut q), None);
    }

    #[test]
    fn removing_and_moving_keep_the_cursor_on_its_track() {
        let mut q = Queue::fixed(&[1, 2, 3, 4, 5], rng());
        play(&mut q);
        play(&mut q); // cursor on 2 (index 1)
        assert_eq!(q.remove(0).map(|e| e.id), Some(1));
        assert_eq!(q.current_index(), Some(0));
        assert!(q.move_entry(3, 1)); // 2 5 3 4
        assert_eq!(ids(q.order()), [2, 5, 3, 4]);
        assert_eq!(q.current_index(), Some(0));
        assert!(q.move_entry(0, 2)); // 5 3 2 4
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(play(&mut q), Some(4));
        assert!(!q.move_entry(0, 9));
        // Removing the playing entry: the next one follows.
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        q.remove(0);
        assert_eq!(play(&mut q), Some(2));
    }

    #[test]
    fn removed_entries_leave_the_next_shuffle_pass() {
        let mut q = Queue::shuffled(&[1, 2, 3], None, rng());
        q.append(&[9]);
        let idx = q.order().iter().position(|e| e.id == 2).unwrap();
        q.remove(idx);
        let mut seen = HashSet::new();
        for _ in 0..30 {
            seen.insert(play(&mut q).unwrap());
        }
        assert!(!seen.contains(&2));
        assert!(seen.contains(&9));
        assert_eq!(
            q.order().iter().filter(|e| e.id == 9).count(),
            0,
            "manual: once"
        );
    }

    #[test]
    fn forgotten_tracks_leave_order_and_history() {
        let mut q = Queue::fixed(&[1, 2, 3, 4], rng());
        play(&mut q);
        play(&mut q);
        q.forget(1);
        assert!(!q.can_go_back());
        assert_eq!(ids(q.order()), [2, 3, 4]);
        assert_eq!(q.current_index(), Some(0));
        q.forget(3);
        assert_eq!(play(&mut q), Some(4));
    }

    #[test]
    fn many_tracks_can_be_forgotten_at_once() {
        let mut q = Queue::fixed(&(1..=10).collect::<Vec<_>>(), rng());
        for _ in 0..4 {
            play(&mut q);
        }
        q.go_back();
        q.forget_where(|id| id % 2 == 1);
        assert_eq!(ids(q.order()), [2, 4, 6, 8, 10]);
        assert_eq!(q.current_index(), Some(1), "still on 4");
        assert_eq!(q.successor(), Some(2), "back to 2, 3 is gone");
        play(&mut q);
        assert_eq!(play(&mut q), Some(4));
        assert_eq!(play(&mut q), Some(6));
    }

    #[test]
    fn a_track_passed_over_stays_in_the_list() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        assert_eq!(q.successor(), Some(2));
        q.pass_over(2);
        assert_eq!(play(&mut q), Some(3));
        assert_eq!(ids(q.order()), [1, 2, 3]);
    }

    #[test]
    fn a_restored_queue_resumes_its_current_track() {
        let mut q = Queue::shuffled(&(1..=20).collect::<Vec<_>>(), None, rng());
        for _ in 0..5 {
            play(&mut q);
        }
        q.go_back();
        play(&mut q);
        q.insert_next(&[99]);
        let state = q.state();
        let current = q.current();

        let mut r = Queue::restore(state.clone(), true, rng());
        assert_eq!(r.state(), state);
        assert_eq!(r.successor(), current, "the current track is due again");
        assert_eq!(play(&mut r), current);
        assert_eq!(play(&mut r), Some(99));
        let mut r = Queue::restore(state, false, rng());
        assert_eq!(r.successor(), Some(99), "without resume the order goes on");
    }

    #[test]
    fn a_resumed_track_that_is_gone_moves_on_to_the_next() {
        let mut q = Queue::fixed(&[1, 2, 3], rng());
        play(&mut q);
        play(&mut q);
        let mut r = Queue::restore(q.state(), true, rng());
        assert_eq!(r.successor(), Some(2));
        r.forget(2);
        assert_eq!(play(&mut r), Some(3));
        let mut r = Queue::restore(q.state(), true, rng());
        r.pass_over(2);
        assert_eq!(play(&mut r), Some(3), "unreachable: also the next one");
    }

    #[test]
    fn version_changes_with_the_order() {
        let mut q = Queue::fixed(&[1, 2], rng());
        let v = q.version();
        play(&mut q);
        assert_eq!(q.version(), v, "a start is no order change");
        q.append(&[3]);
        assert!(q.version() > v);
    }
}
