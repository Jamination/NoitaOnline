//! Lockstep input delay: the frames between capturing an input and simulating it. It has to
//! hide the input's trip client -> server -> every client. The session starts with a delay
//! derived from the lobby round trips; with `--input-delay auto` it then follows what the
//! network needs at the session's frame rate (`ClientMsg::Perf` reports, every ~60 frames):
//!
//! * The network bound: the worst client round trip (`Perf::rtt_us`, the client's pings), plus
//!   a quarter of it and 2 ms of headroom, in frames of the session's pace (the slowest
//!   client's frame time without its tick waits), plus one frame on lossy links (a lost
//!   input or tick packet is repeated one frame later).
//! * Ticks arriving late (the client that waited least still waited `RAISE_STALLS` times)
//!   raise the delay by one frame only while it is below that bound. Above it the inputs
//!   provably arrive in time, so the waits come from frame pacing: in lockstep every client
//!   waits whenever another one runs a frame behind (hitches, a slower machine), whatever the
//!   delay. Measured (`run/e2e/full`, 4 clients on one machine at ~23 fps): 15.3 / 13.7 /
//!   16.4 / 15.4 % of frames stalled at delays 1 / 2 / 3 / 4.
//! * A delay above the bound for `LOWER_ROUNDS` consecutive rounds is lowered by one frame,
//!   down to the bound (never below 1), whatever the stalls.
//!
//! Changing the delay never affects determinism: every client simulates the ticks the server
//! relays, whatever frame their inputs were captured in. A client that switches to a longer
//! delay sends its current input for one extra frame; one that switches to a shorter delay
//! merges two frames' inputs into one (`init.lua`).

use std::collections::{BTreeMap, VecDeque};

pub const FRAME_US: u64 = 16_667;
/// On top of the round trip: the clients' and the server's own processing time (thread
/// wakes, the server's receive loop).
const START_MARGIN_US: u64 = 2_000;
pub const MAX_DELAY: u16 = 60;
/// A report window (~60 frames) in which even the client that waited least waited this many
/// times for a tick raises the delay (below the network bound).
const RAISE_STALLS: u32 = 3;
/// Consecutive rounds above the network bound before lowering (one round can be a hitch).
const LOWER_ROUNDS: u32 = 3;
/// Rounds ignored after a change (its reports still describe the old delay).
const COOLDOWN_ROUNDS: u32 = 2;
/// Each client's round trip is the smallest of its last this many reports (one delayed pong
/// is not the network).
const RTT_REPORTS: usize = 3;
/// Packet loss is measured over this many rounds ...
const LOSS_ROUNDS: usize = 8;
/// ... from at least this many packets to one client ...
const LOSS_MIN_PACKETS: u64 = 100;
/// ... and a link losing at least this share (per mille) of them is lossy.
const LOSSY_PERMILLE: u64 = 10;

/// The start delay for a session whose slowest link has round trip `rtt_us`: enough frames
/// for an input to reach every client before they simulate its frame at 60 fps.
pub fn initial(rtt_us: u64) -> u16 {
    ((rtt_us + START_MARGIN_US).div_ceil(FRAME_US) as u16).clamp(1, MAX_DELAY)
}

/// Frames that hide a round trip of `rtt_us` at a pace of `pace_us` per frame, plus one on
/// lossy links.
pub fn network_bound(rtt_us: u64, pace_us: u64, lossy: bool) -> u16 {
    let trip = rtt_us + rtt_us / 4 + START_MARGIN_US;
    let frames = trip.div_ceil(pace_us.max(1_000)).max(1) + lossy as u64;
    frames.min(MAX_DELAY as u64) as u16
}

/// One client's report window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub frames: u32,
    /// Mean frame interval, tick waits included.
    pub avg_frame_us: u32,
    /// Time blocked waiting for ticks.
    pub wait_us: u32,
    /// Frames whose tick had not arrived when the client needed it.
    pub stalls: u32,
    /// The client's latest ping round trip (0: none yet).
    pub rtt_us: u32,
    /// The server's packets to this client so far: sent, and acknowledged by it.
    pub packets_sent: u64,
    pub packets_acked: u64,
}

impl Window {
    /// Frame time without tick waits: what the client itself needs per frame.
    fn work_us(&self) -> u64 {
        let frames = self.frames.max(1) as u64;
        (self.avg_frame_us as u64).saturating_sub(self.wait_us as u64 / frames)
    }
}

#[derive(Default)]
struct Link {
    rtts: VecDeque<u32>,
    /// (sent, acked) at the end of the last `LOSS_ROUNDS` rounds.
    packets: VecDeque<(u64, u64)>,
}

impl Link {
    fn rtt_us(&self) -> Option<u64> {
        self.rtts.iter().min().map(|r| *r as u64)
    }

    fn lossy(&self) -> bool {
        let (Some(first), Some(last)) = (self.packets.front(), self.packets.back()) else { return false };
        let sent = last.0.saturating_sub(first.0);
        let lost = sent.saturating_sub(last.1.saturating_sub(first.1));
        sent >= LOSS_MIN_PACKETS && lost * 1000 >= sent * LOSSY_PERMILLE
    }
}

pub struct Controller {
    delay: u16,
    adaptive: bool,
    /// Lobby round trip, until the clients report their own.
    start_rtt_us: u64,
    pending: BTreeMap<u8, Window>,
    links: BTreeMap<u8, Link>,
    above_rounds: u32,
    cooldown: u32,
}

impl Controller {
    pub fn new(delay: u16, adaptive: bool, start_rtt_us: u64) -> Self {
        Self { delay, adaptive, start_rtt_us, pending: BTreeMap::new(), links: BTreeMap::new(), above_rounds: 0, cooldown: 0 }
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
        let link = self.links.entry(player).or_default();
        if w.rtt_us > 0 {
            if link.rtts.len() == RTT_REPORTS {
                link.rtts.pop_front();
            }
            link.rtts.push_back(w.rtt_us);
        }
        if link.packets.len() == LOSS_ROUNDS + 1 {
            link.packets.pop_front();
        }
        link.packets.push_back((w.packets_sent, w.packets_acked));
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
        let links = active.iter().filter_map(|p| self.links.get(p));
        let rtt = links.clone().filter_map(Link::rtt_us).max().unwrap_or(self.start_rtt_us);
        let lossy = links.clone().any(Link::lossy);
        // The session runs at the slowest client's pace (in lockstep the others wait for it).
        let pace = round.iter().map(Window::work_us).max()?;
        let bound = network_bound(rtt, pace, lossy);
        // The client that waited least. The others may wait only because another client
        // runs slower; when even it waits and the delay is below what the network needs, the
        // inputs travel longer than the delay.
        let best_stalls = round.iter().map(|w| w.stalls).min()?;
        if self.delay < bound && best_stalls >= RAISE_STALLS {
            self.above_rounds = 0;
            self.delay += 1;
            self.cooldown = COOLDOWN_ROUNDS;
            return Some((self.delay, "ticks arrived late"));
        }
        self.above_rounds = if self.delay > bound { self.above_rounds + 1 } else { 0 };
        if self.above_rounds >= LOWER_ROUNDS {
            self.above_rounds = 0;
            self.delay -= 1;
            self.cooldown = COOLDOWN_ROUNDS;
            return Some((self.delay, "above the network's need"));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window at frame interval `avg_ms` with `stalls` waits of `wait_ms` in total.
    fn win(avg_ms: u32, stalls: u32, wait_ms: u32, rtt_us: u32) -> Window {
        Window { frames: 60, avg_frame_us: avg_ms * 1000, wait_us: wait_ms * 1000, stalls, rtt_us, packets_sent: 0, packets_acked: 0 }
    }

    /// One round: every player of `ws` reports; the decision.
    fn round(c: &mut Controller, ws: &[Window]) -> Option<(u16, &'static str)> {
        let active: Vec<u8> = (0..ws.len() as u8).collect();
        let mut out = None;
        for (p, w) in ws.iter().enumerate() {
            out = c.report(p as u8, *w, &active);
        }
        out
    }

    #[test]
    fn initial_delay_covers_the_round_trip() {
        assert_eq!(initial(300), 1); // localhost
        assert_eq!(initial(14_000), 1);
        assert_eq!(initial(15_000), 2);
        assert_eq!(initial(200_000), 13); // 100 ms each way
        assert_eq!(initial(10_000_000), MAX_DELAY);
    }

    #[test]
    fn network_bound_counts_frames_of_the_session_pace() {
        assert_eq!(network_bound(200, 44_000, false), 1); // localhost at 23 fps
        assert_eq!(network_bound(200, 16_667, false), 1);
        assert_eq!(network_bound(200_000, 45_000, false), 6); // 250 + 2 ms in 45 ms frames
        assert_eq!(network_bound(200_000, 45_000, true), 7);
        assert_eq!(network_bound(200_000, 16_667, false), 16);
        assert_eq!(network_bound(200, 0, false), 3); // pace floored at 1 ms
    }

    #[test]
    fn pacing_stalls_never_raise_on_a_lan() {
        // run/e2e/full: 4 clients at ~44 ms per frame, every one of them waiting several
        // times per window for whichever client hitched; localhost round trips.
        let mut c = Controller::new(1, true, 150);
        for i in 0..200u32 {
            let ws = [win(44, 3 + i % 5, 20, 180), win(44, 5, 30, 200), win(44, 8, 60, 170), win(44, 20, 120, 190)];
            assert_eq!(round(&mut c, &ws), None);
        }
        // A hitch of every client (all wait, huge frames) does not raise either.
        assert_eq!(round(&mut c, &[win(330, 8, 45, 200), win(330, 7, 245, 200)]), None);
        assert_eq!(c.delay(), 1);
    }

    #[test]
    fn a_pacing_raised_delay_returns_to_the_minimum_despite_stalls() {
        let mut c = Controller::new(4, true, 150);
        let ws = [win(44, 5, 40, 200), win(44, 16, 80, 200)];
        let mut changes = vec![];
        for _ in 0..40 {
            if let Some(ch) = round(&mut c, &ws) {
                changes.push(ch);
            }
        }
        let lowered = (3, "above the network's need");
        assert_eq!(changes, vec![lowered, (2, lowered.1), (1, lowered.1)]);
        assert_eq!(c.delay(), 1);
    }

    #[test]
    fn lowering_needs_consecutive_rounds_above_the_bound() {
        // 100 ms each way at 45 ms frames: bound 6. A round at a hitch's pace (bound 3) is
        // not a reason to lower; a slower pace that lasts is.
        let mut c = Controller::new(6, true, 200_000);
        let normal = [win(45, 0, 0, 200_000), win(45, 2, 10, 200_000)];
        let slow = [win(120, 0, 0, 200_000), win(120, 1, 5, 200_000)];
        for _ in 0..10 {
            assert_eq!(round(&mut c, &normal), None);
            assert_eq!(round(&mut c, &normal), None);
            assert_eq!(round(&mut c, &slow), None);
        }
        assert_eq!(c.delay(), 6);
        assert_eq!(round(&mut c, &normal), None);
        for _ in 0..LOWER_ROUNDS - 1 {
            assert_eq!(round(&mut c, &slow), None);
        }
        assert_eq!(round(&mut c, &slow), Some((5, "above the network's need")));
    }

    #[test]
    fn late_ticks_raise_up_to_the_network_bound_only() {
        // 100 ms each way; the clients need 18 ms per frame but wait for ticks on every
        // frame, so their frames take 25 ms: the bound uses the 18 ms (14 frames), not 25.
        let mut c = Controller::new(10, true, 200_000);
        let late = [win(25, 60, 420, 200_000), win(25, 55, 400, 200_000)];
        let mut changes = vec![];
        for _ in 0..100 {
            if let Some((d, _)) = round(&mut c, &late) {
                changes.push(d);
            }
        }
        assert_eq!(changes, (11..=14).collect::<Vec<u16>>());
        assert_eq!(network_bound(200_000, 18_000, false), 14);
    }

    #[test]
    fn a_lossy_link_adds_a_frame() {
        let mut c = Controller::new(6, true, 200_000);
        let mut sent = 0;
        let mut d = vec![];
        for _ in 0..40 {
            sent += 120;
            // 5 % of the server's packets to client 1 are never acknowledged.
            let lossy = Window { packets_sent: sent, packets_acked: sent * 95 / 100, ..win(45, 5, 30, 200_000) };
            let clean = Window { packets_sent: sent, packets_acked: sent, ..win(45, 4, 20, 200_000) };
            if let Some((x, _)) = round(&mut c, &[clean, lossy]) {
                d.push(x);
            }
        }
        assert_eq!(d, vec![7]);
        // Without loss it would have stayed at 6 (the stalls are not the network's).
        let mut c = Controller::new(6, true, 200_000);
        for _ in 0..40 {
            assert_eq!(round(&mut c, &[win(45, 5, 30, 200_000), win(45, 4, 20, 200_000)]), None);
        }
    }

    #[test]
    fn decides_once_every_player_reported_then_cools_down() {
        let mut c = Controller::new(2, true, 200_000);
        let late = win(20, 50, 300, 200_000);
        assert_eq!(c.report(0, late, &[0, 1]), None);
        assert_eq!(c.report(1, late, &[0, 1]), Some((3, "ticks arrived late")));
        // Cooldown: the next two rounds still describe the old delay.
        for _ in 0..COOLDOWN_ROUNDS {
            c.report(0, late, &[0, 1]);
            assert_eq!(c.report(1, late, &[0, 1]), None);
        }
        c.report(0, late, &[0, 1]);
        assert_eq!(c.report(1, late, &[0, 1]), Some((4, "ticks arrived late")));
    }

    #[test]
    fn one_slow_pong_is_not_the_network() {
        // Client 1's latest ping took 80 ms (a scheduling hiccup); its two before 0.2 ms.
        let mut c = Controller::new(1, true, 150);
        round(&mut c, &[win(44, 0, 0, 200), win(44, 0, 0, 200)]);
        round(&mut c, &[win(44, 0, 0, 200), win(44, 0, 0, 200)]);
        assert_eq!(round(&mut c, &[win(44, 10, 50, 200), win(44, 10, 50, 80_000)]), None);
        assert_eq!(c.delay(), 1);
        // Three in a row are the network: 80 ms + 22 ms in 44 ms frames needs 3.
        round(&mut c, &[win(44, 10, 50, 200), win(44, 10, 50, 80_000)]);
        assert_eq!(round(&mut c, &[win(44, 10, 50, 200), win(44, 10, 50, 80_000)]), Some((2, "ticks arrived late")));
    }

    #[test]
    fn fixed_delay_never_changes() {
        let mut f = Controller::new(0, false, 0);
        assert_eq!(f.report(0, win(20, 60, 600, 300_000), &[0]), None);
        assert_eq!(f.delay(), 0);
    }
}
