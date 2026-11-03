//! Client side of the redundant lockstep streams (shared by the game client and simclient).

use crate::{ClientMsg, Tick, STREAM_BUDGET};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Default)]
pub struct ClientStream {
    /// Unacknowledged local inputs, the first for frame `inputs_first`.
    inputs_first: u32,
    inputs: Vec<Vec<u8>>,
    /// Ticks received and not yet consumed, with the time each arrived.
    ticks: BTreeMap<u32, (Tick, Instant)>,
    /// Next tick frame not yet received (all earlier received or consumed).
    tick_ack: u32,
}

impl ClientStream {
    pub fn new(first_input_frame: u32) -> Self {
        Self { inputs_first: first_input_frame, ..Default::default() }
    }

    /// Queue the local input for `frame`. Frames must be pushed in order without gaps.
    pub fn push_input(&mut self, frame: u32, data: Vec<u8>) {
        let next = self.inputs_first + self.inputs.len() as u32;
        if frame < next {
            return;
        }
        // Fill any gap with repeats of the latest input so the server never waits forever.
        while self.inputs_first + (self.inputs.len() as u32) < frame {
            let fill = self.inputs.last().cloned().unwrap_or_default();
            self.inputs.push(fill);
        }
        self.inputs.push(data);
    }

    pub fn has_unacked_inputs(&self) -> bool {
        !self.inputs.is_empty()
    }

    pub fn tick_ack(&self) -> u32 {
        self.tick_ack
    }

    /// Apply a server `Ticks` datagram: drop acknowledged inputs, store new ticks.
    /// Returns true if anything new arrived.
    pub fn on_ticks(&mut self, input_ack: u32, first: u32, ticks: Vec<Tick>) -> bool {
        if input_ack > self.inputs_first {
            let n = ((input_ack - self.inputs_first) as usize).min(self.inputs.len());
            self.inputs.drain(..n);
            self.inputs_first += n as u32;
            if self.inputs.is_empty() {
                self.inputs_first = self.inputs_first.max(input_ack);
            }
        }
        let mut new = false;
        let now = Instant::now();
        for (i, t) in ticks.into_iter().enumerate() {
            let f = first + i as u32;
            if f >= self.tick_ack && !self.ticks.contains_key(&f) {
                self.ticks.insert(f, (t, now));
                new = true;
            }
        }
        while self.ticks.contains_key(&self.tick_ack) {
            self.tick_ack += 1;
        }
        new
    }

    /// Take the tick for `frame` if it has arrived.
    pub fn take_tick(&mut self, frame: u32) -> Option<Tick> {
        self.take_tick_timed(frame).map(|(t, _)| t)
    }

    /// Take the tick for `frame` if it has arrived, with the time it arrived.
    pub fn take_tick_timed(&mut self, frame: u32) -> Option<(Tick, Instant)> {
        self.ticks.remove(&frame)
    }

    /// Datagram repeating every unacknowledged input (up to the stream budget).
    pub fn inputs_msg(&self) -> ClientMsg {
        let mut used = 0;
        let mut inputs: Vec<Vec<u8>> = Vec::new();
        for i in &self.inputs {
            let len = crate::input_encoded_len(inputs.last().map(Vec::as_slice), i);
            if used + len > STREAM_BUDGET && !inputs.is_empty() {
                break;
            }
            used += len;
            inputs.push(i.clone());
        }
        ClientMsg::Inputs { tick_ack: self.tick_ack, first: self.inputs_first, inputs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acks_trim_inputs_and_ticks_accumulate() {
        let mut s = ClientStream::new(3);
        for f in 3..8 {
            s.push_input(f, vec![f as u8]);
        }
        match s.inputs_msg() {
            ClientMsg::Inputs { first, inputs, .. } => {
                assert_eq!(first, 3);
                assert_eq!(inputs.len(), 5);
            }
            _ => unreachable!(),
        }
        // Server acks inputs up to 6 (needs 6 next) and sends ticks 0..2.
        assert!(s.on_ticks(6, 0, vec![vec![], vec![(0, vec![1])]]));
        match s.inputs_msg() {
            ClientMsg::Inputs { tick_ack, first, inputs } => {
                assert_eq!((tick_ack, first), (2, 6));
                assert_eq!(inputs, vec![vec![6], vec![7]]);
            }
            _ => unreachable!(),
        }
        // Redundant repeat of an old tick changes nothing; a gap keeps the ack at 2.
        assert!(!s.on_ticks(6, 1, vec![vec![(0, vec![1])]]));
        assert!(s.on_ticks(6, 3, vec![vec![]]));
        assert_eq!(s.tick_ack(), 2);
        assert!(s.on_ticks(6, 2, vec![vec![]]));
        assert_eq!(s.tick_ack(), 4);
        assert_eq!(s.take_tick(1), Some(vec![(0, vec![1])]));
        assert_eq!(s.take_tick(1), None);
    }

    #[test]
    fn gap_in_pushed_inputs_is_filled_with_repeats() {
        let mut s = ClientStream::new(0);
        s.push_input(0, vec![1]);
        s.push_input(3, vec![4]);
        match s.inputs_msg() {
            ClientMsg::Inputs { inputs, .. } => assert_eq!(inputs, vec![vec![1], vec![1], vec![1], vec![4]]),
            _ => unreachable!(),
        }
    }
}
