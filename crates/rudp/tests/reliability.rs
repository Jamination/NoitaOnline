//! Reliability-core behaviour over a simulated lossy, reordering, duplicating link.

use rudp::connection::{ChannelKind, Config, Connection, SendError};
use rudp::linksim::{LinkConditions, LinkSim};

const TICK_US: u64 = 16_667;

struct Pair {
    a: Connection,
    b: Connection,
    ab: LinkSim<()>,
    ba: LinkSim<()>,
    now: u64,
    out: Vec<Vec<u8>>,
}

impl Pair {
    fn new(cfg: Config, cond: LinkConditions, seed: u64) -> Self {
        Self {
            a: Connection::new(cfg.clone(), 0xfeed, 0),
            b: Connection::new(cfg, 0xfeed, 0),
            ab: LinkSim::new(cond, seed),
            ba: LinkSim::new(cond, seed.wrapping_mul(31).wrapping_add(7)),
            now: 0,
            out: Vec::new(),
        }
    }

    fn step(&mut self) {
        self.now += TICK_US;
        self.a.generate_packets(self.now, &mut self.out);
        for p in self.out.drain(..) {
            self.ab.push((), p, self.now);
        }
        self.b.generate_packets(self.now, &mut self.out);
        for p in self.out.drain(..) {
            self.ba.push((), p, self.now);
        }
        while let Some((_, p)) = self.ab.pop_due(self.now) {
            self.b.process_packet(&p, self.now).unwrap();
        }
        while let Some((_, p)) = self.ba.pop_due(self.now) {
            self.a.process_packet(&p, self.now).unwrap();
        }
    }

    fn drain_b(&mut self) -> Vec<(u8, Vec<u8>)> {
        std::iter::from_fn(|| self.b.receive()).collect()
    }
}

fn bad_link() -> LinkConditions {
    LinkConditions { loss: 0.3, latency_us: 40_000, jitter_us: 30_000, duplicate: 0.05 }
}

fn payload(i: u32, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = i.wrapping_mul(2654435761).max(1);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.push(x as u8);
    }
    v
}

#[test]
fn reliable_messages_arrive_once_in_order_despite_loss_reorder_dup() {
    let mut p = Pair::new(Config::default(), bad_link(), 1);
    let n = 3000u32;
    let mut sent = 0u32;
    let mut got = Vec::new();
    for _ in 0..20_000 {
        // Keep feeding a few messages per tick, like a game would.
        for _ in 0..3 {
            if sent < n {
                p.a.send(0, &sent.to_le_bytes()).unwrap();
                sent += 1;
            }
        }
        p.step();
        got.extend(p.drain_b());
        if got.len() == n as usize {
            break;
        }
    }
    let ids: Vec<u32> =
        got.iter().map(|(c, d)| {
            assert_eq!(*c, 0);
            u32::from_le_bytes(d[..].try_into().unwrap())
        }).collect();
    assert_eq!(ids, (0..n).collect::<Vec<_>>());
    assert!(p.a.stats.messages_resent > 0, "30% loss must force retransmissions");
    assert!(p.b.stats.duplicate_packets > 0, "duplicates were injected and must be filtered");
}

#[test]
fn large_messages_fragment_and_reassemble_interleaved_with_small_ones() {
    let mut p = Pair::new(Config::default(), bad_link(), 2);
    let big_sizes = [1_199, 1_200, 5_000, 250_000, 1 << 20];
    let mut expected = Vec::new();
    for (i, &len) in big_sizes.iter().enumerate() {
        let big = payload(i as u32, len);
        p.a.send(0, &big).unwrap();
        expected.push(big);
        let small = payload(1000 + i as u32, 10);
        p.a.send(0, &small).unwrap();
        expected.push(small);
    }
    p.a.send(0, &[]).unwrap();
    expected.push(Vec::new());
    let mut got = Vec::new();
    for _ in 0..50_000 {
        p.step();
        got.extend(p.drain_b().into_iter().map(|(_, d)| d));
        if got.len() == expected.len() {
            break;
        }
    }
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(&expected) {
        assert_eq!(g.len(), e.len());
        assert!(g == e, "payload corrupted (len {})", e.len());
    }
}

#[test]
fn every_datagram_respects_mtu() {
    let cfg = Config::default();
    let mtu = cfg.mtu;
    let mut a = Connection::new(cfg, 1, 0);
    a.send(0, &payload(1, 100_000)).unwrap();
    for i in 0..50 {
        a.send(1, &payload(i, 900)).unwrap();
    }
    let mut out = Vec::new();
    a.generate_packets(1, &mut out);
    assert!(out.len() > 80);
    assert!(out.iter().all(|p| p.len() <= mtu), "max {}", out.iter().map(Vec::len).max().unwrap());
}

#[test]
fn unreliable_rejects_oversize_and_never_retransmits() {
    let mut p = Pair::new(Config::default(), LinkConditions::new(1.0, 0), 3);
    assert_eq!(p.a.send(1, &vec![0u8; 1200]), Err(SendError::TooLarge));
    assert_eq!(p.a.send(9, b"x"), Err(SendError::NoSuchChannel));
    p.a.send(1, b"lost forever").unwrap();
    p.step();
    // Restore the link: the unreliable message must not come back.
    p.ab = LinkSim::new(LinkConditions::new(0.0, 0), 4);
    p.ba = LinkSim::new(LinkConditions::new(0.0, 0), 5);
    for _ in 0..100 {
        p.step();
    }
    assert!(p.drain_b().is_empty());
}

#[test]
fn delivery_resumes_after_total_outage() {
    let mut p = Pair::new(Config::default(), LinkConditions::new(1.0, 20), 6);
    for i in 0..50u32 {
        p.a.send(0, &i.to_le_bytes()).unwrap();
    }
    for _ in 0..60 {
        p.step(); // one second of 100% loss
    }
    assert!(p.drain_b().is_empty());
    p.ab = LinkSim::new(LinkConditions::new(0.0, 20), 7);
    p.ba = LinkSim::new(LinkConditions::new(0.0, 20), 8);
    let mut got = Vec::new();
    for _ in 0..200 {
        p.step();
        got.extend(p.drain_b());
    }
    assert_eq!(got.len(), 50);
    assert_eq!(p.a.unacked_reliable(), 0, "sender must see every message acked");
}

#[test]
fn channels_are_ordered_independently() {
    let cfg = Config {
        channels: vec![ChannelKind::ReliableOrdered, ChannelKind::ReliableOrdered],
        ..Config::default()
    };
    let mut p = Pair::new(cfg, bad_link(), 9);
    for i in 0..500u32 {
        p.a.send((i % 2) as u8, &i.to_le_bytes()).unwrap();
    }
    let mut got = Vec::new();
    for _ in 0..5_000 {
        p.step();
        got.extend(p.drain_b());
    }
    for ch in 0..2u8 {
        let ids: Vec<u32> = got
            .iter()
            .filter(|(c, _)| *c == ch)
            .map(|(_, d)| u32::from_le_bytes(d[..].try_into().unwrap()))
            .collect();
        let want: Vec<u32> = (0..500).filter(|i| i % 2 == ch as u32).collect();
        assert_eq!(ids, want);
    }
}

#[test]
fn sequence_numbers_wrap_without_loss_of_order() {
    // 70k messages and >65,536 packets force both packet seq and message ids to wrap.
    let mut p = Pair::new(Config::default(), LinkConditions::new(0.1, 0), 10);
    let n = 70_000u32;
    let mut sent = 0;
    let mut next = 0u32;
    for _ in 0..200_000 {
        if sent < n {
            p.a.send(0, &sent.to_le_bytes()).unwrap();
            sent += 1;
        }
        p.step();
        for (_, d) in p.drain_b() {
            assert_eq!(u32::from_le_bytes(d[..].try_into().unwrap()), next);
            next += 1;
        }
        if next == n {
            break;
        }
    }
    assert_eq!(next, n);
    assert!(p.a.stats.packets_sent > 65_536);
}

#[test]
fn rtt_estimate_tracks_link_latency() {
    let mut p = Pair::new(Config::default(), LinkConditions::new(0.0, 50), 11);
    for i in 0..600u32 {
        p.a.send(0, &i.to_le_bytes()).unwrap();
        p.step();
    }
    // 50 ms each way plus up to one tick of send/ack batching.
    let rtt = p.a.rtt_us();
    assert!((100_000..=140_000).contains(&rtt), "rtt {rtt}");
}

#[test]
fn peer_silence_times_out() {
    let cfg = Config { timeout_us: 1_000_000, ..Config::default() };
    let mut p = Pair::new(cfg, LinkConditions::new(1.0, 0), 12);
    for _ in 0..59 {
        p.step();
    }
    assert!(!p.a.timed_out(p.now));
    for _ in 0..2 {
        p.step();
    }
    assert!(p.a.timed_out(p.now));
}

#[test]
fn malformed_packets_are_rejected_without_side_effects() {
    let mut a = Connection::new(Config::default(), 5, 0);
    let mut b = Connection::new(Config::default(), 5, 0);
    a.send(0, b"hello").unwrap();
    let mut out = Vec::new();
    a.generate_packets(1, &mut out);
    let good = out.pop().unwrap();
    // Wrong token.
    let mut wrong = good.clone();
    wrong[1] ^= 0xff;
    assert!(b.process_packet(&wrong, 2).is_err());
    // Truncated mid-message.
    assert!(b.process_packet(&good[..good.len() - 2], 2).is_err());
    assert!(b.receive().is_none());
    b.process_packet(&good, 3).unwrap();
    assert_eq!(b.receive(), Some((0, b"hello".to_vec())));
}
