//! Deterministic lossy/latent link simulator for tests and in-process network conditioning.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkConditions {
    /// Probability in [0,1] that a datagram is dropped.
    pub loss: f64,
    /// One-way delay added to every datagram (microseconds).
    pub latency_us: u64,
    /// Extra uniformly random delay in [0, jitter_us] (may reorder datagrams).
    pub jitter_us: u64,
    /// Probability in [0,1] that a delivered datagram is duplicated.
    pub duplicate: f64,
}

impl LinkConditions {
    pub fn new(loss: f64, latency_ms: u64) -> Self {
        Self { loss, latency_us: latency_ms * 1000, jitter_us: 0, duplicate: 0.0 }
    }

    /// Parse `loss=0.05,latency=100,jitter=10,dup=0.01` (latency/jitter in ms).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut c = Self::new(0.0, 0);
        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (k, v) = part.split_once('=').ok_or_else(|| format!("bad item '{part}'"))?;
            let f: f64 = v.parse().map_err(|_| format!("bad number in '{part}'"))?;
            match k {
                "loss" => c.loss = f,
                "latency" => c.latency_us = (f * 1000.0) as u64,
                "jitter" => c.jitter_us = (f * 1000.0) as u64,
                "dup" => c.duplicate = f,
                _ => return Err(format!("unknown key '{k}'")),
            }
        }
        Ok(c)
    }
}

/// xorshift64* — small, fast, seedable.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

pub struct LinkSim<A> {
    cond: LinkConditions,
    rng: Rng,
    order: u64,
    queue: BinaryHeap<Reverse<(u64, u64, Item<A>)>>,
}

struct Item<A>(A, Vec<u8>);

impl<A> PartialEq for Item<A> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl<A> Eq for Item<A> {}
impl<A> PartialOrd for Item<A> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<A> Ord for Item<A> {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl<A: Clone> LinkSim<A> {
    pub fn new(cond: LinkConditions, seed: u64) -> Self {
        Self { cond, rng: Rng::new(seed), order: 0, queue: BinaryHeap::new() }
    }

    pub fn push(&mut self, addr: A, data: Vec<u8>, now: u64) {
        if self.rng.unit() < self.cond.loss {
            return;
        }
        let copies = if self.rng.unit() < self.cond.duplicate { 2 } else { 1 };
        for _ in 0..copies {
            let at = now + self.cond.latency_us + self.rng.below(self.cond.jitter_us + 1);
            self.order += 1;
            self.queue.push(Reverse((at, self.order, Item(addr.clone(), data.clone()))));
        }
    }

    pub fn pop_due(&mut self, now: u64) -> Option<(A, Vec<u8>)> {
        match self.queue.peek() {
            Some(Reverse((at, _, _))) if *at <= now => {
                let Reverse((_, _, Item(a, d))) = self.queue.pop().unwrap();
                Some((a, d))
            }
            _ => None,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec() {
        let c = LinkConditions::parse("loss=0.05,latency=100,jitter=5").unwrap();
        assert_eq!(c.latency_us, 100_000);
        assert_eq!(c.jitter_us, 5_000);
        assert!((c.loss - 0.05).abs() < 1e-12);
        assert!(LinkConditions::parse("bogus=1").is_err());
    }

    #[test]
    fn delays_and_drops() {
        let mut s = LinkSim::new(LinkConditions::new(0.5, 10), 7);
        for i in 0..1000u32 {
            s.push((), i.to_le_bytes().to_vec(), 0);
        }
        assert!(s.pop_due(9_999).is_none(), "nothing before latency elapses");
        let mut n = 0;
        while s.pop_due(10_000).is_some() {
            n += 1;
        }
        assert!((400..600).contains(&n), "about half dropped, got {n}");
    }
}
