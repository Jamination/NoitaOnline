//! Lockstep input relay: gathers each player's input for a simulation frame and emits one
//! authoritative tick per frame, in order, once every active player has contributed.
//! Emitted ticks are kept so they can be repeated to clients until acknowledged.

use protocol::Tick;
use std::collections::{BTreeMap, HashMap};

pub struct Relay {
    next_frame: u32,
    input_delay: u32,
    active: Vec<u8>,
    inputs: BTreeMap<u32, HashMap<u8, Vec<u8>>>,
    /// Per player: next input frame not yet received (all earlier frames are present or
    /// were never needed).
    contiguous: HashMap<u8, u32>,
    /// Emitted ticks starting at frame `history_base`.
    history: Vec<Tick>,
    history_base: u32,
}

/// Inputs further than this many frames ahead of the relay are ignored (bounds memory).
const MAX_AHEAD: u32 = 600;

impl Relay {
    pub fn new(players: Vec<u8>, input_delay: u32) -> Self {
        let contiguous = players.iter().map(|p| (*p, input_delay)).collect();
        Self {
            next_frame: 0,
            input_delay,
            active: players,
            inputs: BTreeMap::new(),
            contiguous,
            history: Vec::new(),
            history_base: 0,
        }
    }

    pub fn next_frame(&self) -> u32 {
        self.next_frame
    }

    pub fn active(&self) -> &[u8] {
        &self.active
    }

    /// Next input frame still needed from `player` (what the server acknowledges).
    pub fn input_ack(&self, player: u8) -> u32 {
        self.contiguous.get(&player).copied().unwrap_or(0)
    }

    /// Record `player`'s input for `frame`. Duplicates (redundant resends) and stale or
    /// out-of-window frames are ignored.
    pub fn submit(&mut self, player: u8, frame: u32, data: &[u8]) {
        if !self.active.contains(&player) || frame < self.next_frame.max(self.input_delay) || frame > self.next_frame + MAX_AHEAD {
            return;
        }
        let slot = self.inputs.entry(frame).or_default();
        if slot.contains_key(&player) {
            return;
        }
        slot.insert(player, data.to_vec());
        let c = self.contiguous.entry(player).or_insert(self.input_delay);
        while self.inputs.get(c).is_some_and(|s| s.contains_key(&player)) || *c < self.next_frame {
            *c += 1;
        }
    }

    /// Remove a player: from the next unsent frame on, ticks no longer wait for or include it.
    pub fn remove(&mut self, player: u8) {
        self.active.retain(|p| *p != player);
        self.contiguous.remove(&player);
        for slot in self.inputs.values_mut() {
            slot.remove(&player);
        }
    }

    /// Emit every frame that is complete; returns how many were emitted. Frames before
    /// `input_delay` need no inputs (clients submit for frame f + delay while simulating f).
    pub fn advance(&mut self) -> usize {
        let mut n = 0;
        while !self.active.is_empty() {
            let f = self.next_frame;
            let ready = f < self.input_delay
                || self.active.iter().all(|p| self.inputs.get(&f).is_some_and(|s| s.contains_key(p)));
            if !ready {
                break;
            }
            let mut slot = self.inputs.remove(&f).unwrap_or_default();
            let mut tick: Tick = self.active.iter().map(|p| (*p, slot.remove(p).unwrap_or_default())).collect();
            tick.sort_by_key(|(p, _)| *p);
            self.history.push(tick);
            self.next_frame += 1;
            n += 1;
        }
        n
    }

    /// Ticks from `from` (inclusive) onward, at most `budget` encoded bytes.
    pub fn ticks_from(&self, from: u32, budget: usize) -> (u32, Vec<Tick>) {
        let from = from.max(self.history_base);
        let mut out = Vec::new();
        let mut used = 0;
        let start = (from - self.history_base) as usize;
        for t in self.history.iter().skip(start) {
            let len = protocol::tick_encoded_len(out.last(), t);
            if used + len > budget && !out.is_empty() {
                break;
            }
            used += len;
            out.push(t.clone());
        }
        (from, out)
    }

    /// Drop history every client has acknowledged.
    pub fn trim(&mut self, min_ack: u32) {
        if min_ack > self.history_base {
            let n = ((min_ack - self.history_base) as usize).min(self.history.len());
            self.history.drain(..n);
            self.history_base += n as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_frames_flow_then_wait_for_everyone() {
        let mut r = Relay::new(vec![0, 1], 2);
        assert_eq!(r.advance(), 2);
        let (first, t) = r.ticks_from(0, 1000);
        assert_eq!(first, 0);
        assert!(t.iter().all(|i| i.len() == 2 && i.iter().all(|(_, d)| d.is_empty())));
        r.submit(0, 2, &[7]);
        assert_eq!(r.advance(), 0, "player 1 missing");
        r.submit(1, 2, &[8]);
        assert_eq!(r.advance(), 1);
        assert_eq!(r.ticks_from(2, 1000).1, vec![vec![(0, vec![7]), (1, vec![8])]]);
    }

    #[test]
    fn duplicates_and_out_of_order_inputs_build_contiguous_ack() {
        let mut r = Relay::new(vec![0], 1);
        assert_eq!(r.input_ack(0), 1);
        r.submit(0, 3, &[3]);
        assert_eq!(r.input_ack(0), 1, "gap at 1");
        r.submit(0, 1, &[1]);
        r.submit(0, 1, &[9]); // redundant resend must not overwrite
        assert_eq!(r.input_ack(0), 2);
        r.submit(0, 2, &[2]);
        assert_eq!(r.input_ack(0), 4);
        assert_eq!(r.advance(), 4);
        let (_, t) = r.ticks_from(1, 1000);
        assert_eq!(t, vec![vec![(0, vec![1])], vec![(0, vec![2])], vec![(0, vec![3])]]);
        r.submit(0, 2, &[5]); // stale
        r.submit(0, 5000, &[5]); // too far ahead
        assert_eq!(r.input_ack(0), 4);
    }

    #[test]
    fn removed_player_no_longer_blocks_or_appears() {
        let mut r = Relay::new(vec![0, 1, 2], 0);
        r.submit(0, 0, &[1]);
        r.submit(2, 0, &[3]);
        assert_eq!(r.advance(), 0);
        r.remove(1);
        assert_eq!(r.advance(), 1);
        assert_eq!(r.ticks_from(0, 1000).1, vec![vec![(0, vec![1]), (2, vec![3])]]);
    }

    #[test]
    fn ticks_respect_budget_and_trim() {
        let mut r = Relay::new(vec![0], 0);
        for f in 0..100u32 {
            r.submit(0, f, &[f as u8; 50]);
        }
        assert_eq!(r.advance(), 100);
        let (_, t) = r.ticks_from(0, 1100);
        let used: usize = t.iter().enumerate().map(|(i, x)| protocol::tick_encoded_len(i.checked_sub(1).map(|j| &t[j]), x)).sum();
        assert!(t.len() < 100 && used <= 1100);
        r.trim(60);
        let (first, t) = r.ticks_from(10, 100_000);
        assert_eq!((first, t.len()), (60, 40), "trimmed history starts at the min ack");
    }
}
