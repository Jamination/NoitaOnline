//! Noita Online game protocol: messages exchanged between the headless server and clients
//! over the rudp transport.
//!
//! Channel 0 (reliable, ordered) carries session control, state-hash reports and logs.
//! Channel 1 (unreliable) carries the lockstep streams: every client datagram repeats all
//! of its inputs the server has not yet acknowledged, and every server datagram repeats all
//! ticks the client has not yet acknowledged. A lost datagram therefore costs one frame
//! interval instead of a retransmission round trip, and there is no head-of-line blocking.

pub mod stream;

use rudp::wire::{DecodeError, DecodeResult, Reader, Writer};

pub const CH_RELIABLE: u8 = 0;
pub const CH_UNRELIABLE: u8 = 1;

/// Bumped whenever the message format or lockstep rules change; server and clients must match.
/// 5: every input carries its client's render camera (the client's input trailer).
pub const GAME_PROTOCOL_VERSION: u32 = 5;

/// Budget for one unreliable lockstep datagram's payload (fits rudp's unreliable limit).
pub const STREAM_BUDGET: usize = 1100;

pub fn transport_config() -> rudp::Config {
    rudp::Config {
        channels: vec![rudp::ChannelKind::ReliableOrdered, rudp::ChannelKind::Unreliable],
        ..rudp::Config::default()
    }
}

/// Per-chunk hash entry: chunk table index and 64-bit content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHash {
    pub index: u32,
    pub hash: u64,
}

/// A tracked entity position at a hash checkpoint. Coordinates are raw f32 bits so
/// comparisons are exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityPos {
    pub id: u32,
    /// 0 = player, 1 = enemy, 2 = other tracked.
    pub kind: u8,
    pub x_bits: u32,
    pub y_bits: u32,
}

/// One frame's inputs from every active player, ordered by player id.
pub type Tick = Vec<(u8, Vec<u8>)>;

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello { protocol: u32, game_build: String, mod_hash: u64, name: String },
    /// Lockstep stream (unreliable). `tick_ack`: next tick frame the client still needs
    /// (all earlier ticks received). `inputs[i]` is the local input for frame `first + i`.
    Inputs { tick_ack: u32, first: u32, inputs: Vec<Vec<u8>> },
    /// State at the end of simulation frame `frame`: hashes of the chunks scheduled for this
    /// frame and, on entity checkpoints, the entity digest plus tracked positions.
    StateHash { frame: u32, entities: Option<u64>, chunks: Vec<ChunkHash>, positions: Vec<EntityPos> },
    /// Periodic client-side performance report. `wait_us`: time blocked waiting for ticks;
    /// `stalls`: frames whose tick had not arrived when the game needed it (waits over 1 ms);
    /// `margin_min_us`: over the other frames, the shortest time between a tick's arrival
    /// and the game needing it (`u32::MAX`: no such frame).
    Perf { frames: u32, avg_frame_us: u32, p99_frame_us: u32, max_frame_us: u32, wait_us: u32, stalls: u32, margin_min_us: u32 },
    /// Free-form log line forwarded to the server log.
    Log { text: String },
    Ping { t: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Welcome { player: u8, max_players: u8 },
    Lobby { players: Vec<(u8, String)>, needed: u8 },
    /// Session starts: every client starts a new game with this seed and these parameters.
    Start { seed: u32, players: Vec<(u8, String)>, input_delay: u16, session_id: u64, params: Vec<u8> },
    /// Lockstep stream (unreliable). `input_ack`: next input frame the server still needs
    /// from this client. `ticks[i]` is the tick for frame `first + i`.
    Ticks { input_ack: u32, first: u32, ticks: Vec<Tick> },
    PlayerLeft { player: u8 },
    Pong { t: u64 },
    Kick { reason: String },
    /// New input delay: from its next frame on, each client sends its input for frame
    /// f + `delay` while simulating f (the server adapts the delay to the measured margins).
    InputDelay { delay: u16 },
}

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        match self {
            ClientMsg::Hello { protocol, game_build, mod_hash, name } => {
                w.u8(1).u32(*protocol).str(game_build).u64(*mod_hash).str(name);
            }
            ClientMsg::Inputs { tick_ack, first, inputs } => {
                w.u8(2).varint(*tick_ack as u64).varint(*first as u64).varint(inputs.len() as u64);
                // Delta coding: 0 = same as the previous input, 1 = blob follows.
                for (i, input) in inputs.iter().enumerate() {
                    if i > 0 && inputs[i - 1] == *input {
                        w.u8(0);
                    } else {
                        w.u8(1).blob(input);
                    }
                }
            }
            ClientMsg::StateHash { frame, entities, chunks, positions } => {
                w.u8(3).varint(*frame as u64);
                match entities {
                    Some(e) => w.u8(1).u64(*e),
                    None => w.u8(0),
                };
                w.varint(chunks.len() as u64);
                for c in chunks {
                    w.varint(c.index as u64).u64(c.hash);
                }
                w.varint(positions.len() as u64);
                for p in positions {
                    w.varint(p.id as u64).u8(p.kind).u32(p.x_bits).u32(p.y_bits);
                }
            }
            ClientMsg::Perf { frames, avg_frame_us, p99_frame_us, max_frame_us, wait_us, stalls, margin_min_us } => {
                w.u8(4)
                    .varint(*frames as u64)
                    .varint(*avg_frame_us as u64)
                    .varint(*p99_frame_us as u64)
                    .varint(*max_frame_us as u64)
                    .varint(*wait_us as u64)
                    .varint(*stalls as u64)
                    .varint(*margin_min_us as u64);
            }
            ClientMsg::Log { text } => {
                w.u8(5).str(text);
            }
            ClientMsg::Ping { t } => {
                w.u8(6).u64(*t);
            }
        }
        w.finish()
    }

    pub fn decode(data: &[u8]) -> DecodeResult<Self> {
        let mut r = Reader::new(data);
        let m = match r.u8()? {
            1 => ClientMsg::Hello {
                protocol: r.u32()?,
                game_build: r.str()?.to_owned(),
                mod_hash: r.u64()?,
                name: r.str()?.to_owned(),
            },
            2 => {
                let tick_ack = r.varint()? as u32;
                let first = r.varint()? as u32;
                let n = bounded(r.varint()?, &r, 1)?;
                let mut inputs: Vec<Vec<u8>> = Vec::with_capacity(n);
                for _ in 0..n {
                    match r.u8()? {
                        0 => {
                            let prev = inputs.last().ok_or(DecodeError)?.clone();
                            inputs.push(prev);
                        }
                        1 => inputs.push(r.blob()?.to_vec()),
                        _ => return Err(DecodeError),
                    }
                }
                ClientMsg::Inputs { tick_ack, first, inputs }
            }
            3 => {
                let frame = r.varint()? as u32;
                let entities = match r.u8()? {
                    0 => None,
                    1 => Some(r.u64()?),
                    _ => return Err(DecodeError),
                };
                let n = bounded(r.varint()?, &r, 9)?;
                let mut chunks = Vec::with_capacity(n);
                for _ in 0..n {
                    chunks.push(ChunkHash { index: r.varint()? as u32, hash: r.u64()? });
                }
                let n = bounded(r.varint()?, &r, 10)?;
                let mut positions = Vec::with_capacity(n);
                for _ in 0..n {
                    positions.push(EntityPos {
                        id: r.varint()? as u32,
                        kind: r.u8()?,
                        x_bits: r.u32()?,
                        y_bits: r.u32()?,
                    });
                }
                ClientMsg::StateHash { frame, entities, chunks, positions }
            }
            4 => ClientMsg::Perf {
                frames: r.varint()? as u32,
                avg_frame_us: r.varint()? as u32,
                p99_frame_us: r.varint()? as u32,
                max_frame_us: r.varint()? as u32,
                wait_us: r.varint()? as u32,
                stalls: r.varint()? as u32,
                margin_min_us: r.varint()? as u32,
            },
            5 => ClientMsg::Log { text: r.str()?.to_owned() },
            6 => ClientMsg::Ping { t: r.u64()? },
            _ => return Err(DecodeError),
        };
        if !r.is_empty() {
            return Err(DecodeError);
        }
        Ok(m)
    }
}

/// Reject element counts that cannot possibly fit in the remaining bytes (each element
/// needs at least `min_elem` bytes), so hostile lengths cannot trigger huge allocations.
fn bounded(n: u64, r: &Reader, min_elem: usize) -> DecodeResult<usize> {
    if n > (r.remaining() / min_elem.max(1)) as u64 {
        return Err(DecodeError);
    }
    Ok(n as usize)
}

fn put_roster(w: &mut Writer, players: &[(u8, String)]) {
    w.varint(players.len() as u64);
    for (id, name) in players {
        w.u8(*id).str(name);
    }
}

fn get_roster(r: &mut Reader) -> DecodeResult<Vec<(u8, String)>> {
    let n = bounded(r.varint()?, r, 2)?;
    let mut players = Vec::with_capacity(n);
    for _ in 0..n {
        players.push((r.u8()?, r.str()?.to_owned()));
    }
    Ok(players)
}

/// Encoded size of `t` inside a `Ticks` message whose previous tick is `prev` (entries equal
/// to the same player's entry in `prev` cost one byte).
pub fn tick_encoded_len(prev: Option<&Tick>, t: &Tick) -> usize {
    1 + t
        .iter()
        .map(|(id, d)| {
            if same_as_prev(prev, *id, d) {
                1
            } else {
                1 + varint_len(d.len() as u64) + d.len()
            }
        })
        .sum::<usize>()
}

fn same_as_prev(prev: Option<&Tick>, id: u8, data: &[u8]) -> bool {
    prev.is_some_and(|p| p.iter().any(|(pid, pd)| *pid == id && pd.as_slice() == data))
}

/// Encoded size of `input` inside an `Inputs` message whose previous input is `prev`.
pub fn input_encoded_len(prev: Option<&[u8]>, input: &[u8]) -> usize {
    if prev == Some(input) {
        1
    } else {
        1 + varint_len(input.len() as u64) + input.len()
    }
}

/// Player ids are < 0x80; the high bit of the id byte marks "same input as this player's
/// entry in the previous tick of this message" (no blob follows).
const TICK_REPEAT_FLAG: u8 = 0x80;

pub fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        match self {
            ServerMsg::Welcome { player, max_players } => {
                w.u8(1).u8(*player).u8(*max_players);
            }
            ServerMsg::Lobby { players, needed } => {
                w.u8(2).u8(*needed);
                put_roster(&mut w, players);
            }
            ServerMsg::Start { seed, players, input_delay, session_id, params } => {
                w.u8(3).u32(*seed).u16(*input_delay).u64(*session_id);
                put_roster(&mut w, players);
                w.blob(params);
            }
            ServerMsg::Ticks { input_ack, first, ticks } => {
                w.u8(4).varint(*input_ack as u64).varint(*first as u64).varint(ticks.len() as u64);
                let mut prev: Option<&Tick> = None;
                for t in ticks {
                    w.u8(t.len() as u8);
                    for (id, data) in t {
                        debug_assert!(*id < TICK_REPEAT_FLAG);
                        if same_as_prev(prev, *id, data) {
                            w.u8(*id | TICK_REPEAT_FLAG);
                        } else {
                            w.u8(*id).blob(data);
                        }
                    }
                    prev = Some(t);
                }
            }
            ServerMsg::PlayerLeft { player } => {
                w.u8(5).u8(*player);
            }
            ServerMsg::Pong { t } => {
                w.u8(6).u64(*t);
            }
            ServerMsg::Kick { reason } => {
                w.u8(7).str(reason);
            }
            ServerMsg::InputDelay { delay } => {
                w.u8(8).u16(*delay);
            }
        }
        w.finish()
    }

    pub fn decode(data: &[u8]) -> DecodeResult<Self> {
        let mut r = Reader::new(data);
        let m = match r.u8()? {
            1 => ServerMsg::Welcome { player: r.u8()?, max_players: r.u8()? },
            2 => {
                let needed = r.u8()?;
                ServerMsg::Lobby { players: get_roster(&mut r)?, needed }
            }
            3 => {
                let seed = r.u32()?;
                let input_delay = r.u16()?;
                let session_id = r.u64()?;
                let players = get_roster(&mut r)?;
                let params = r.blob()?.to_vec();
                ServerMsg::Start { seed, players, input_delay, session_id, params }
            }
            4 => {
                let input_ack = r.varint()? as u32;
                let first = r.varint()? as u32;
                let n = bounded(r.varint()?, &r, 1)?;
                let mut ticks: Vec<Tick> = Vec::with_capacity(n);
                for _ in 0..n {
                    let m = r.u8()? as usize;
                    let mut t = Vec::with_capacity(m);
                    for _ in 0..m {
                        let b = r.u8()?;
                        let id = b & !TICK_REPEAT_FLAG;
                        if b & TICK_REPEAT_FLAG != 0 {
                            let prev = ticks.last().ok_or(DecodeError)?;
                            let data = prev.iter().find(|(pid, _)| *pid == id).ok_or(DecodeError)?.1.clone();
                            t.push((id, data));
                        } else {
                            t.push((id, r.blob()?.to_vec()));
                        }
                    }
                    ticks.push(t);
                }
                ServerMsg::Ticks { input_ack, first, ticks }
            }
            5 => ServerMsg::PlayerLeft { player: r.u8()? },
            6 => ServerMsg::Pong { t: r.u64()? },
            7 => ServerMsg::Kick { reason: r.str()?.to_owned() },
            8 => ServerMsg::InputDelay { delay: r.u16()? },
            _ => return Err(DecodeError),
        };
        if !r.is_empty() {
            return Err(DecodeError);
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_roundtrip() {
        let msgs = vec![
            ClientMsg::Hello { protocol: 1, game_build: "b".into(), mod_hash: 9, name: "n".into() },
            ClientMsg::Inputs { tick_ack: 70_000, first: 70_003, inputs: vec![vec![1, 2, 3], vec![]] },
            ClientMsg::StateHash {
                frame: 60,
                entities: Some(3),
                chunks: vec![ChunkHash { index: 131328, hash: 42 }],
                positions: vec![EntityPos { id: 7, kind: 1, x_bits: 1.5f32.to_bits(), y_bits: 0 }],
            },
            ClientMsg::StateHash { frame: 61, entities: None, chunks: vec![], positions: vec![] },
            ClientMsg::Perf { frames: 60, avg_frame_us: 16666, p99_frame_us: 20000, max_frame_us: 30000, wait_us: 5, stalls: 2, margin_min_us: u32::MAX },
            ClientMsg::Log { text: "hi".into() },
            ClientMsg::Ping { t: 5 },
        ];
        for m in msgs {
            assert_eq!(ClientMsg::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn server_messages_roundtrip() {
        let msgs = vec![
            ServerMsg::Welcome { player: 3, max_players: 8 },
            ServerMsg::Lobby { players: vec![(0, "a".into())], needed: 4 },
            ServerMsg::Start { seed: 1, players: vec![(0, "a".into()), (1, "b".into())], input_delay: 4, session_id: 77, params: vec![9] },
            ServerMsg::Ticks { input_ack: 15, first: 12, ticks: vec![vec![(0, vec![1]), (1, vec![])], vec![]] },
            ServerMsg::PlayerLeft { player: 2 },
            ServerMsg::Pong { t: 8 },
            ServerMsg::Kick { reason: "x".into() },
            ServerMsg::InputDelay { delay: 2 },
        ];
        for m in msgs {
            assert_eq!(ServerMsg::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn tick_len_matches_encoding_including_repeats() {
        let t1: Tick = vec![(0, vec![1; 6]), (3, vec![2; 200])];
        let t2: Tick = vec![(0, vec![1; 6]), (3, vec![9; 3])];
        let empty = ServerMsg::Ticks { input_ack: 0, first: 0, ticks: vec![] }.encode().len();
        let one = ServerMsg::Ticks { input_ack: 0, first: 0, ticks: vec![t1.clone()] }.encode().len();
        let two = ServerMsg::Ticks { input_ack: 0, first: 0, ticks: vec![t1.clone(), t2.clone()] }.encode().len();
        assert_eq!(one - empty, tick_encoded_len(None, &t1));
        assert_eq!(two - one, tick_encoded_len(Some(&t1), &t2));
        assert_eq!(tick_encoded_len(Some(&t1), &t2), 1 + 1 + (1 + 1 + 3), "player 0 repeats");
    }

    #[test]
    fn repeated_inputs_and_ticks_roundtrip_compactly() {
        let same = vec![7u8; 23];
        let inputs = ClientMsg::Inputs { tick_ack: 1, first: 2, inputs: vec![same.clone(); 30] };
        let enc = inputs.encode();
        assert!(enc.len() < 23 + 2 + 30 + 8, "{} bytes", enc.len());
        assert_eq!(ClientMsg::decode(&enc).unwrap(), inputs);
        let tick: Tick = (0..4).map(|p| (p, same.clone())).collect();
        let ticks = ServerMsg::Ticks { input_ack: 0, first: 0, ticks: vec![tick; 30] };
        assert_eq!(ServerMsg::decode(&ticks.encode()).unwrap(), ticks);
        // A repeat flag in the first tick (nothing to repeat) is malformed.
        let mut w = Writer::new();
        w.u8(4).varint(0).varint(0).varint(1).u8(1).u8(0x80);
        assert!(ServerMsg::decode(&w.finish()).is_err());
    }

    #[test]
    fn hostile_counts_and_trailing_bytes_rejected() {
        // StateHash claiming 2^40 chunks in a tiny packet.
        let mut w = Writer::new();
        w.u8(3).varint(1).u8(1).u64(0).varint(1 << 40);
        assert!(ClientMsg::decode(&w.finish()).is_err());
        let mut enc = ServerMsg::Pong { t: 1 }.encode();
        enc.push(0);
        assert!(ServerMsg::decode(&enc).is_err());
    }
}
