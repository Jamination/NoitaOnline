//! Lockstep input delay: the frames between capturing an input and simulating it. It has to
//! hide the input's trip client -> server -> every client. The session starts with a delay
//! derived from the lobby round trips; with `--input-delay auto` it then follows the tick
//! margins the clients report (`ClientMsg::Perf`): one frame more while ticks arrive late,
//! one frame less while every tick arrives more than a frame before it is needed.
//!
//! Changing the delay never affects determinism: every client simulates the ticks the server
//! relays, whatever frame their inputs were captured in. A client that switches to a longer
//! delay sends its current input for one extra frame; one that switches to a shorter delay
//! merges two frames' inputs into one (`init.lua`).

use std::cmp::Reverse;
use std::collections::BTreeMap;

pub const FRAME_US: u64 = 16_667;
/// On top of the round trip at the start: the clients' and the server's own processing time
/// (thread wakes, the server's receive loop).
const START_MARGIN_US: u64 = 2_000;
pub const MAX_DELAY: u16 = 60;
/// A report window (~60 frames) in which even the client that waited least waited this many
/// times for a tick raises the delay.
const RAISE_STALLS: u32 = 3;
/// Lowering needs that client's earliest tick to have arrived this long before it was
/// needed, in this many consecutive report rounds (doubled after each raise, up to the cap).
const LOWER_MARGIN_US: u64 = FRAME_US + 2_000;
const LOWER_ROUNDS: u32 = 10;
const LOWER_ROUNDS_MAX: u32 = 160;
/// Rounds ignored after a change (its reports still describe the old delay).
const COOLDOWN_ROUNDS: u32 = 2;

/// The start delay for a session whose slowest link has round trip `rtt_us`: enough frames
/// for an input to reach every client before they simulate its frame.
pub fn initial(rtt_us: u64) -> u16 {
    ((rtt_us + START_MARGIN_US).div_ceil(FRAME_US) as u16).clamp(1, MAX_DELAY)
}

/// One client's report window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Frames whose tick had not arrived when the client needed it.
    pub stalls: u32,
    /// Shortest arrival-to-use time of the other frames' ticks (`u32::MAX`: none).
    pub margin_min_us: u32,
}

pub struct Controller {
    delay: u16,
    adaptive: bool,
    pending: BTreeMap<u8, Window>,
    clean_rounds: u32,
    lower_rounds: u32,
    cooldown: u32,
}

impl Controller {
    pub fn new(delay: u16, adaptive: bool) -> Self {
        Self { delay, adaptive, pending: BTreeMap::new(), clean_rounds: 0, lower_rounds: LOWER_ROUNDS, cooldown: 0 }
    }

    pub fn delay(&self) -> u16 {
        self.delay
    }

    /// Record `player`'s window; once every active player reported, decide. Returns the new
    /// delay and the reason when it changes.
    pub fn report(&mut self, player: u8, w: Window, active: &[u8]) -> Option<(u16, &'static str)> {
        if !self.adaptive || !active.contains(&player) {
            return None;
        }
        self.pending.insert(player, w);
        if !active.iter().all(|p| self.pending.contains_key(p)) {
            return None;
        }
        let round: Vec<Window> = active.iter().filter_map(|p| self.pending.get(p).copied()).collect();
        self.pending.clear();
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        // The client that waited least. The others may wait only because it runs slower (in
        // lockstep the faster clients wait for the slowest one's inputs, whatever the delay);
        // when even it waits, the inputs travel longer than the delay.
        let best = round.iter().min_by_key(|w| (w.stalls, Reverse(w.margin_min_us)))?;
        if best.stalls >= RAISE_STALLS {
            self.clean_rounds = 0;
            if self.delay >= MAX_DELAY {
                return None;
            }
            self.delay += 1;
            self.lower_rounds = (self.lower_rounds * 2).min(LOWER_ROUNDS_MAX);
            self.cooldown = COOLDOWN_ROUNDS;
            return Some((self.delay, "ticks arrived late"));
        }
        let early = best.stalls == 0 && best.margin_min_us != u32::MAX && best.margin_min_us as u64 >= LOWER_MARGIN_US;
        self.clean_rounds = if early { self.clean_rounds + 1 } else { 0 };
        if self.clean_rounds >= self.lower_rounds && self.delay > 1 {
            self.clean_rounds = 0;
            self.delay -= 1;
            self.cooldown = COOLDOWN_ROUNDS;
            return Some((self.delay, "ticks arrived over a frame early"));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATE: Window = Window { stalls: 5, margin_min_us: 1000 };
    const EARLY: Window = Window { stalls: 0, margin_min_us: 30_000 };
    const TIGHT: Window = Window { stalls: 0, margin_min_us: 4_000 };

    #[test]
    fn initial_delay_covers_the_round_trip() {
        assert_eq!(initial(300), 1); // localhost
        assert_eq!(initial(14_000), 1);
        assert_eq!(initial(15_000), 2);
        assert_eq!(initial(200_000), 13); // 100 ms each way
        assert_eq!(initial(10_000_000), MAX_DELAY);
    }

    #[test]
    fn decides_once_every_player_reported() {
        let mut c = Controller::new(2, true);
        assert_eq!(c.report(0, LATE, &[0, 1]), None);
        assert_eq!(c.report(1, LATE, &[0, 1]), Some((3, "ticks arrived late")));
        // Cooldown: the next two rounds still describe the old delay.
        for _ in 0..2 {
            c.report(0, LATE, &[0, 1]);
            assert_eq!(c.report(1, LATE, &[0, 1]), None);
        }
        c.report(0, LATE, &[0, 1]);
        assert_eq!(c.report(1, LATE, &[0, 1]), Some((4, "ticks arrived late")));
    }

    #[test]
    fn a_slower_client_alone_does_not_raise() {
        // Player 0 waits every frame for player 1's inputs; player 1 never waits.
        let mut c = Controller::new(1, true);
        for _ in 0..50 {
            c.report(0, LATE, &[0, 1]);
            assert_eq!(c.report(1, TIGHT, &[0, 1]), None);
        }
        assert_eq!(c.delay(), 1);
    }

    #[test]
    fn lowers_after_sustained_early_ticks_with_backoff() {
        let mut c = Controller::new(4, true);
        let mut rounds = 0;
        while c.report(0, EARLY, &[0]).is_none() {
            rounds += 1;
        }
        assert_eq!((rounds + 1, c.delay()), (LOWER_ROUNDS, 3));
        // A raise doubles the rounds needed for the next lowering.
        for _ in 0..COOLDOWN_ROUNDS {
            c.report(0, EARLY, &[0]);
        }
        assert_eq!(c.report(0, LATE, &[0]), Some((4, "ticks arrived late")));
        for _ in 0..COOLDOWN_ROUNDS {
            c.report(0, EARLY, &[0]);
        }
        let mut rounds = 0;
        while c.report(0, EARLY, &[0]).is_none() {
            rounds += 1;
        }
        assert_eq!(rounds + 1, 2 * LOWER_ROUNDS);
    }

    #[test]
    fn never_below_one_and_fixed_delay_never_changes() {
        let mut c = Controller::new(1, true);
        for _ in 0..1000 {
            assert_eq!(c.report(0, EARLY, &[0]), None);
        }
        let mut f = Controller::new(0, false);
        assert_eq!(f.report(0, LATE, &[0]), None);
        assert_eq!(f.delay(), 0);
    }
}
