// SPDX-License-Identifier: GPL-2.0-or-later
//! The music playlist: a shuffled order of the disc's songs, in the way a
//! car-soccer menu plays them.
//!
//! Every song plays once before any plays again. When the order runs out it
//! is shuffled afresh, and the first song of the new order is never the one
//! that just finished, so the repeat the shuffle allows between two rounds
//! cannot happen back to back. A player stepping through with the TRACK row
//! walks the same order forwards and backwards.
//!
//! Pure integer maths on a xorshift32 stream, so the order is a function of
//! the seed and the host tests can check it.

/// Songs in one round. The disc carries six (`game/src/music_map.rs`).
pub const MAX_SONGS: usize = 8;

/// A shuffled round of `len` songs and where in it the player is.
#[derive(Clone, Debug)]
pub struct Playlist {
    order: [u8; MAX_SONGS],
    len: u8,
    pos: u8,
    rng: u32,
}

impl Playlist {
    /// A playlist of songs `0..len`, shuffled from `seed`. `len` is clamped
    /// to `1..=MAX_SONGS`.
    pub fn new(len: usize, seed: u32) -> Self {
        let len = len.clamp(1, MAX_SONGS) as u8;
        let mut list = Playlist {
            order: [0; MAX_SONGS],
            len,
            pos: 0,
            // xorshift32 never leaves zero.
            rng: if seed == 0 { 0x9E37_79B9 } else { seed },
        };
        list.shuffle(None);
        list
    }

    /// The song playing now.
    pub fn current(&self) -> usize {
        self.order[self.pos as usize] as usize
    }

    /// Step to the next song of the round, starting a new round after the
    /// last. Returns the song to play.
    pub fn next(&mut self) -> usize {
        if self.pos + 1 < self.len {
            self.pos += 1;
        } else {
            let last = self.current();
            self.shuffle(Some(last));
            self.pos = 0;
        }
        self.current()
    }

    /// Step back to the previous song of the round, wrapping to the end of
    /// it from the first. Returns the song to play.
    pub fn prev(&mut self) -> usize {
        self.pos = if self.pos == 0 { self.len - 1 } else { self.pos - 1 };
        self.current()
    }

    fn step(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    /// Fisher-Yates over `0..len`. With `not_first` set, a result that opens
    /// with that song swaps it with the second place (a round of one has no
    /// choice and repeats).
    fn shuffle(&mut self, not_first: Option<usize>) {
        let n = self.len as usize;
        for i in 0..n {
            self.order[i] = i as u8;
        }
        for i in (1..n).rev() {
            let j = (self.step() >> 8) as usize % (i + 1);
            self.order.swap(i, j);
        }
        if n > 1 && not_first == Some(self.order[0] as usize) {
            self.order.swap(0, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_round(songs: &[usize], len: usize) -> bool {
        let mut seen = [false; MAX_SONGS];
        songs.iter().all(|&s| s < len && !core::mem::replace(&mut seen[s], true))
    }

    #[test]
    fn every_round_plays_each_song_once() {
        for seed in 1..200u32 {
            let mut list = Playlist::new(6, seed.wrapping_mul(2_654_435_761));
            for _ in 0..5 {
                let mut round = [0usize; 6];
                for slot in round.iter_mut() {
                    *slot = list.current();
                    list.next();
                }
                assert!(is_round(&round, 6), "seed {seed}: {round:?}");
            }
        }
    }

    #[test]
    fn the_first_song_of_a_new_round_is_not_the_one_that_just_ended() {
        for seed in 1..2000u32 {
            let mut list = Playlist::new(6, seed);
            let mut last = list.current();
            for _ in 0..60 {
                let now = list.next();
                assert_ne!(now, last, "seed {seed}");
                last = now;
            }
        }
    }

    #[test]
    fn different_seeds_give_different_orders() {
        let order = |seed| {
            let mut list = Playlist::new(6, seed);
            let mut out = [0usize; 6];
            for slot in out.iter_mut() {
                *slot = list.current();
                list.next();
            }
            out
        };
        let first = order(1);
        assert!((2..40).any(|seed| order(seed) != first));
    }

    #[test]
    fn prev_walks_back_over_what_next_walked() {
        let mut list = Playlist::new(6, 77);
        let mut forward = [0usize; 6];
        forward[0] = list.current();
        for slot in forward.iter_mut().skip(1) {
            *slot = list.next();
        }
        for want in forward.iter().rev().skip(1) {
            assert_eq!(list.prev(), *want);
        }
        // Back past the first song wraps to the last of the round.
        assert_eq!(list.prev(), forward[5]);
    }

    #[test]
    fn a_single_song_just_repeats() {
        let mut list = Playlist::new(1, 5);
        assert_eq!(list.current(), 0);
        assert_eq!(list.next(), 0);
        assert_eq!(list.prev(), 0);
    }

    #[test]
    fn a_zero_seed_still_shuffles() {
        let mut list = Playlist::new(6, 0);
        let mut round = [0usize; 6];
        for slot in round.iter_mut() {
            *slot = list.current();
            list.next();
        }
        assert!(is_round(&round, 6));
    }
}
