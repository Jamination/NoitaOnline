//! Cross-client state verification: collects per-frame state-hash reports from every
//! player and compares world chunks and tracked entity positions exactly. Frames restart at
//! 0 with every run (a new world after an all-dead game over), so reports are kept per
//! (run, frame) and totals are also kept per run.

use protocol::{ChunkHash, EntityPos};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct Report {
    /// Entity digest on entity checkpoints only.
    pub entities: Option<u64>,
    pub chunks: Vec<ChunkHash>,
    pub positions: Vec<EntityPos>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    pub checkpoints: u64,
    pub chunk_comparisons: u64,
    pub chunk_mismatches: u64,
    pub position_comparisons: u64,
    pub position_mismatches: u64,
    pub entity_hash_mismatches: u64,
    /// Checkpoints where some chunk was loaded on only a subset of clients.
    pub partial_chunk_sets: u64,
}

pub struct Verifier {
    /// Reports by (run, frame).
    pending: BTreeMap<(u32, u32), HashMap<u8, Report>>,
    /// All runs.
    pub totals: Totals,
    pub run_totals: BTreeMap<u32, Totals>,
    out: Option<std::fs::File>,
    first_mismatch: Option<(u32, u32)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub run: u32,
    pub frame: u32,
    pub players: usize,
    pub common_chunks: usize,
    pub chunk_mismatches: usize,
    pub common_positions: usize,
    pub position_mismatches: usize,
    pub entity_hash_equal: bool,
    pub partial_chunks: usize,
    /// Up to 8 mismatching chunk indices / (kind, entity id) pairs, for diagnosis.
    pub bad_chunks: Vec<u32>,
    pub bad_entities: Vec<(u8, u32)>,
}

impl Verifier {
    pub fn new(out: Option<std::fs::File>) -> Self {
        Self { pending: BTreeMap::new(), totals: Totals::default(), run_totals: BTreeMap::new(), out, first_mismatch: None }
    }

    /// (run, frame) of the first checkpoint with a chunk or position mismatch.
    pub fn first_mismatch(&self) -> Option<(u32, u32)> {
        self.first_mismatch
    }

    /// Record a report of run `run`; once every player in `active` (the run's players) has
    /// reported `frame`, compare and return the outcome. Older incomplete frames are evaluated
    /// with whoever reported (a player that left mid-way must not stall verification).
    pub fn add(&mut self, run: u32, frame: u32, player: u8, report: Report, active: &[u8]) -> Vec<Outcome> {
        self.pending.entry((run, frame)).or_default().insert(player, report);
        let ready: Vec<(u32, u32)> = self
            .pending
            .iter()
            .filter(|((r, f), reps)| *r == run && (active.iter().all(|p| reps.contains_key(p)) || *f + 600 < frame))
            .map(|(k, _)| *k)
            .collect();
        self.evaluate(ready)
    }

    /// Run `run` is over (every client left it): its incomplete frames are evaluated with
    /// whoever reported them.
    pub fn end_run(&mut self, run: u32) -> Vec<Outcome> {
        let keys: Vec<(u32, u32)> = self.pending.keys().filter(|(r, _)| *r == run).copied().collect();
        self.evaluate(keys)
    }

    fn evaluate(&mut self, keys: Vec<(u32, u32)>) -> Vec<Outcome> {
        let mut done = Vec::new();
        for (run, f) in keys {
            let reps = self.pending.remove(&(run, f)).unwrap();
            if reps.len() >= 2 {
                let o = compare(run, f, &reps);
                self.record(&o, &reps);
                done.push(o);
            }
        }
        done
    }

    fn record(&mut self, o: &Outcome, reps: &HashMap<u8, Report>) {
        for t in [&mut self.totals, self.run_totals.entry(o.run).or_default()] {
            t.checkpoints += 1;
            t.chunk_comparisons += o.common_chunks as u64;
            t.chunk_mismatches += o.chunk_mismatches as u64;
            t.position_comparisons += o.common_positions as u64;
            t.position_mismatches += o.position_mismatches as u64;
            if !o.entity_hash_equal {
                t.entity_hash_mismatches += 1;
            }
            if o.partial_chunks > 0 {
                t.partial_chunk_sets += 1;
            }
        }
        if (o.chunk_mismatches > 0 || o.position_mismatches > 0) && self.first_mismatch.is_none() {
            self.first_mismatch = Some((o.run, o.frame));
        }
        if let Some(f) = &mut self.out {
            let mut players: Vec<_> = reps.keys().copied().collect();
            players.sort();
            let _ = writeln!(
                f,
                "{{\"run\":{},\"frame\":{},\"players\":{:?},\"common_chunks\":{},\"chunk_mismatches\":{},\"partial_chunks\":{},\"common_positions\":{},\"position_mismatches\":{},\"entity_hash_equal\":{},\"bad_chunks\":{:?},\"bad_entities\":[{}]}}",
                o.run, o.frame, players, o.common_chunks, o.chunk_mismatches, o.partial_chunks, o.common_positions, o.position_mismatches, o.entity_hash_equal, o.bad_chunks,
                o.bad_entities.iter().map(|(k, id)| format!("[{k},{id}]")).collect::<Vec<_>>().join(",")
            );
        }
    }
}

fn compare(run: u32, frame: u32, reps: &HashMap<u8, Report>) -> Outcome {
    // Chunks: compare those loaded on 2+ clients; count chunks loaded on only some clients.
    let mut chunk_hashes: HashMap<u32, Vec<u64>> = HashMap::new();
    for r in reps.values() {
        for c in &r.chunks {
            chunk_hashes.entry(c.index).or_default().push(c.hash);
        }
    }
    let n = reps.len();
    let mut common_chunks = 0;
    let mut chunk_mismatches = 0;
    let mut partial = 0;
    let mut bad_chunks = Vec::new();
    for (index, hs) in &chunk_hashes {
        if hs.len() < n {
            partial += 1;
        }
        if hs.len() >= 2 {
            common_chunks += 1;
            if hs.iter().any(|h| *h != hs[0]) {
                chunk_mismatches += 1;
                bad_chunks.push(*index);
            }
        }
    }
    bad_chunks.sort_unstable();
    bad_chunks.truncate(8);
    let mut pos: HashMap<(u8, u32), Vec<(u32, u32)>> = HashMap::new();
    for r in reps.values() {
        for p in &r.positions {
            pos.entry((p.kind, p.id)).or_default().push((p.x_bits, p.y_bits));
        }
    }
    let mut common_positions = 0;
    let mut position_mismatches = 0;
    let mut bad_entities = Vec::new();
    for (&(kind, id), v) in &pos {
        let differs = v.len() < n || v.iter().any(|p| *p != v[0]);
        // Kind 2 (all other entities) is only reported in diagnostic runs; it is listed but
        // not counted against the tracked player/enemy positions.
        if kind <= 1 {
            if v.len() >= 2 {
                common_positions += 1;
            }
            // Present on fewer clients than reported (even only one) is a divergence too.
            if differs {
                position_mismatches += 1;
            }
        }
        if differs {
            bad_entities.push((kind, id));
        }
    }
    bad_entities.sort_unstable();
    bad_entities.truncate(8);
    let mut digests = reps.values().filter_map(|r| r.entities);
    let first = digests.next();
    let entity_hash_equal = first.is_none_or(|f| digests.all(|d| d == f));
    Outcome {
        run,
        frame,
        players: n,
        common_chunks,
        chunk_mismatches,
        common_positions,
        position_mismatches,
        entity_hash_equal,
        partial_chunks: partial,
        bad_chunks,
        bad_entities,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(chunks: &[(u32, u64)], pos: &[(u32, u8, f32, f32)]) -> Report {
        Report {
            entities: Some(1),
            chunks: chunks.iter().map(|&(index, hash)| ChunkHash { index, hash }).collect(),
            positions: pos
                .iter()
                .map(|&(id, kind, x, y)| EntityPos { id, kind, x_bits: x.to_bits(), y_bits: y.to_bits() })
                .collect(),
        }
    }

    #[test]
    fn waits_for_all_active_then_counts_exact_mismatches() {
        let mut v = Verifier::new(None);
        let active = [0u8, 1, 2];
        assert!(v.add(0, 60, 0, rep(&[(1, 10), (2, 20)], &[(5, 0, 1.0, 2.0)]), &active).is_empty());
        assert!(v.add(0, 60, 1, rep(&[(1, 10), (2, 21)], &[(5, 0, 1.0, 2.0)]), &active).is_empty());
        let out = v.add(0, 60, 2, rep(&[(1, 10)], &[(5, 0, 1.0, 2.000001)]), &active);
        assert_eq!(
            out,
            vec![Outcome {
                run: 0,
                frame: 60,
                players: 3,
                common_chunks: 2,
                chunk_mismatches: 1,
                common_positions: 1,
                position_mismatches: 1,
                entity_hash_equal: true,
                partial_chunks: 1,
                bad_chunks: vec![2],
                bad_entities: vec![(0, 5)],
            }]
        );
        assert_eq!(v.first_mismatch(), Some((0, 60)));
    }

    #[test]
    fn entity_missing_on_one_client_is_a_mismatch() {
        let mut v = Verifier::new(None);
        let active = [0u8, 1, 2];
        v.add(0, 60, 0, rep(&[], &[(5, 1, 0.0, 0.0)]), &active);
        v.add(0, 60, 1, rep(&[], &[(5, 1, 0.0, 0.0)]), &active);
        let out = v.add(0, 60, 2, rep(&[], &[]), &active);
        assert_eq!(out[0].position_mismatches, 1);
    }

    #[test]
    fn departed_player_does_not_stall_old_frames() {
        let mut v = Verifier::new(None);
        v.add(0, 60, 0, rep(&[(1, 1)], &[]), &[0, 1]);
        v.add(0, 60, 1, rep(&[(1, 1)], &[]), &[0, 1]);
        // Player 1 leaves; frame 120 only gets player 0, then much later frames arrive.
        v.add(0, 120, 0, rep(&[(1, 1)], &[]), &[0, 1]);
        let out = v.add(0, 900, 0, rep(&[(1, 1)], &[]), &[0]);
        // Frame 120 had a single report (nothing to compare) and is dropped; 900 likewise.
        assert!(out.is_empty());
        assert_eq!(v.totals.checkpoints, 1);
    }

    #[test]
    fn runs_are_kept_apart_and_an_ended_run_is_evaluated() {
        let mut v = Verifier::new(None);
        // Run 0: player 1 reported frame 60, then player 0 left the run before reporting it.
        v.add(0, 60, 1, rep(&[(1, 1)], &[]), &[0, 1]);
        v.add(0, 60, 0, rep(&[(1, 1)], &[]), &[0, 1]);
        v.add(0, 120, 1, rep(&[(1, 5)], &[]), &[0, 1]);
        v.add(0, 120, 0, rep(&[(1, 6)], &[]), &[1, 2]);
        // Run 1 restarts at frame 60 with different worlds: never compared with run 0's.
        assert!(v.add(1, 60, 0, rep(&[(1, 2)], &[]), &[0, 1]).is_empty());
        let out = v.add(1, 60, 1, rep(&[(1, 2)], &[]), &[0, 1]);
        assert_eq!((out[0].run, out[0].chunk_mismatches), (1, 0));
        // Run 0's frame 120 waited for player 2, which never reports: evaluated at its end.
        let out = v.end_run(0);
        assert_eq!((out[0].run, out[0].frame, out[0].chunk_mismatches), (0, 120, 1));
        assert_eq!(v.run_totals[&0].chunk_mismatches, 1);
        assert_eq!(v.run_totals[&1].checkpoints, 1);
        assert_eq!(v.totals.checkpoints, 3);
        assert_eq!(v.first_mismatch(), Some((0, 120)));
    }
}
