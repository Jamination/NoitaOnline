//! Transport-agnostic reliable-UDP connection core.
//!
//! Packets carry a 16-bit sequence plus an ack + 32-bit ack history, so every packet
//! acknowledges up to 33 received packets. Messages ride inside packets:
//! * unreliable channels: fire-and-forget, never fragmented;
//! * reliable-ordered channels: per-channel 16-bit message ids, resent on a timer until a
//!   packet containing them is acked, delivered strictly in order, with large messages
//!   split into in-order fragments that the receiver reassembles.
//!
//! Time is passed in explicitly (microseconds) so the core is deterministic under test.

use crate::packet::{DATA_HEADER_LEN, KIND_DATA, KIND_DATA_NOACK};
use crate::seq::{seq_greater, seq_less, SeqBuffer};
use crate::wire::{DecodeError, DecodeResult, Reader, Writer};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Unreliable,
    ReliableOrdered,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum UDP payload we emit.
    pub mtu: usize,
    /// Reliable messages in flight per channel.
    pub reliable_window: u16,
    /// Lower bound on the resend interval.
    pub resend_min_us: u64,
    /// Upper bound on the resend interval.
    pub resend_max_us: u64,
    /// Send an (empty) packet at least this often so the peer gets acks/keepalives.
    pub keepalive_us: u64,
    /// Peer silent for this long => connection timed out.
    pub timeout_us: u64,
    /// Largest reliable message accepted (after reassembly).
    pub max_message_size: usize,
    pub channels: Vec<ChannelKind>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mtu: 1200,
            reliable_window: 1024,
            resend_min_us: 20_000,
            resend_max_us: 500_000,
            keepalive_us: 100_000,
            timeout_us: 10_000_000,
            max_message_size: 16 << 20,
            channels: vec![ChannelKind::ReliableOrdered, ChannelKind::Unreliable],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    NoSuchChannel,
    TooLarge,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_acked: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub messages_resent: u64,
    pub duplicate_packets: u64,
}

const MSG_FLAG_MORE_FRAGMENTS: u8 = 0x40;
const MSG_CHANNEL_MASK: u8 = 0x3f;
const PACKET_HISTORY: usize = 1024;
/// Upper bound on per-message framing: header byte + id + varint length.
const MSG_OVERHEAD: usize = 1 + 2 + 3;

struct SentPacket {
    time: u64,
    /// (channel, message id) of every reliable message carried.
    msgs: Vec<(u8, u16)>,
    acked: bool,
}

struct OutMsg {
    data: Vec<u8>,
    more: bool,
    last_sent: Option<u64>,
}

struct Reliable {
    next_send_id: u16,
    oldest_unacked: u16,
    send_buf: SeqBuffer<OutMsg>,
    /// Messages not yet admitted to the in-flight window.
    backlog: VecDeque<(Vec<u8>, bool)>,
    next_recv_id: u16,
    recv_buf: SeqBuffer<(Vec<u8>, bool)>,
    assembling: Vec<u8>,
    assembling_overflow: bool,
}

enum Channel {
    Unreliable { outgoing: VecDeque<Vec<u8>> },
    Reliable(Box<Reliable>),
}

pub struct Connection {
    cfg: Config,
    token: u64,
    local_seq: u16,
    remote_seq: u16,
    recv_history: u32,
    have_received: bool,
    received_seqs: SeqBuffer<()>,
    sent: SeqBuffer<SentPacket>,
    channels: Vec<Channel>,
    incoming: VecDeque<(u8, Vec<u8>)>,
    rtt_us: f64,
    rtt_var_us: f64,
    rtt_sampled: bool,
    last_recv: u64,
    last_send: u64,
    ack_pending: bool,
    pub stats: Stats,
}

impl Connection {
    pub fn new(cfg: Config, token: u64, now: u64) -> Self {
        assert!(cfg.channels.len() <= MSG_CHANNEL_MASK as usize + 1);
        assert!(cfg.mtu >= 256);
        let window = cfg.reliable_window as usize;
        let channels = cfg
            .channels
            .iter()
            .map(|k| match k {
                ChannelKind::Unreliable => Channel::Unreliable { outgoing: VecDeque::new() },
                ChannelKind::ReliableOrdered => Channel::Reliable(Box::new(Reliable {
                    next_send_id: 0,
                    oldest_unacked: 0,
                    send_buf: SeqBuffer::new(window),
                    backlog: VecDeque::new(),
                    next_recv_id: 0,
                    recv_buf: SeqBuffer::new(window),
                    assembling: Vec::new(),
                    assembling_overflow: false,
                })),
            })
            .collect();
        Self {
            cfg,
            token,
            local_seq: 0,
            remote_seq: 0,
            recv_history: 0,
            have_received: false,
            received_seqs: SeqBuffer::new(PACKET_HISTORY),
            sent: SeqBuffer::new(PACKET_HISTORY),
            channels,
            incoming: VecDeque::new(),
            rtt_us: 100_000.0,
            rtt_var_us: 50_000.0,
            rtt_sampled: false,
            last_recv: now,
            last_send: 0,
            ack_pending: false,
            stats: Stats::default(),
        }
    }

    pub fn token(&self) -> u64 {
        self.token
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Smoothed round-trip time in microseconds.
    pub fn rtt_us(&self) -> u64 {
        self.rtt_us as u64
    }

    pub fn timed_out(&self, now: u64) -> bool {
        now.saturating_sub(self.last_recv) > self.cfg.timeout_us
    }

    /// Payload bytes available for messages in one packet.
    fn packet_budget(&self) -> usize {
        self.cfg.mtu - DATA_HEADER_LEN
    }

    /// Largest fragment payload, chosen so one fragment always fits a packet.
    fn fragment_size(&self) -> usize {
        self.packet_budget() - MSG_OVERHEAD
    }

    /// Number of reliable messages queued or in flight that the peer has not acked.
    pub fn unacked_reliable(&self) -> usize {
        self.channels
            .iter()
            .map(|c| match c {
                Channel::Reliable(r) => {
                    r.next_send_id.wrapping_sub(r.oldest_unacked) as usize + r.backlog.len()
                }
                Channel::Unreliable { .. } => 0,
            })
            .sum()
    }

    /// Earliest time (µs) at which [`Self::generate_packets`] has something to send without new
    /// input (`now` for queued messages and pending acks; else the next reliable resend or the
    /// keepalive) or the connection times out. Lets an event-driven caller sleep until then.
    pub fn next_deadline(&self, now: u64) -> u64 {
        if self.ack_pending {
            return now;
        }
        let resend = self.resend_interval();
        let mut at = (self.last_send + self.cfg.keepalive_us).min(self.last_recv + self.cfg.timeout_us + 1);
        for ch in &self.channels {
            match ch {
                Channel::Unreliable { outgoing } => {
                    if !outgoing.is_empty() {
                        return now;
                    }
                }
                Channel::Reliable(r) => {
                    let in_flight = r.next_send_id.wrapping_sub(r.oldest_unacked);
                    if !r.backlog.is_empty() && in_flight < self.cfg.reliable_window {
                        return now;
                    }
                    let mut id = r.oldest_unacked;
                    while id != r.next_send_id {
                        match r.send_buf.get(id).map(|m| m.last_sent) {
                            Some(None) => return now,
                            Some(Some(t)) => at = at.min(t + resend),
                            None => {}
                        }
                        id = id.wrapping_add(1);
                    }
                }
            }
        }
        at
    }

    pub fn send(&mut self, channel: u8, data: &[u8]) -> Result<(), SendError> {
        let frag = self.fragment_size();
        let max_msg = self.cfg.max_message_size;
        let unreliable_max = self.packet_budget() - MSG_OVERHEAD;
        match self.channels.get_mut(channel as usize) {
            None => Err(SendError::NoSuchChannel),
            Some(Channel::Unreliable { outgoing }) => {
                if data.len() > unreliable_max {
                    return Err(SendError::TooLarge);
                }
                outgoing.push_back(data.to_vec());
                Ok(())
            }
            Some(Channel::Reliable(r)) => {
                if data.len() > max_msg {
                    return Err(SendError::TooLarge);
                }
                if data.is_empty() {
                    r.backlog.push_back((Vec::new(), false));
                } else {
                    let mut chunks = data.chunks(frag).peekable();
                    while let Some(c) = chunks.next() {
                        r.backlog.push_back((c.to_vec(), chunks.peek().is_some()));
                    }
                }
                Ok(())
            }
        }
    }

    /// Pop the next fully received message.
    pub fn receive(&mut self) -> Option<(u8, Vec<u8>)> {
        self.incoming.pop_front()
    }

    fn resend_interval(&self) -> u64 {
        let v = self.rtt_us + 4.0 * self.rtt_var_us;
        (v as u64).clamp(self.cfg.resend_min_us, self.cfg.resend_max_us)
    }

    fn admit_backlog(&mut self) {
        let window = self.cfg.reliable_window;
        for ch in &mut self.channels {
            if let Channel::Reliable(r) = ch {
                while r.next_send_id.wrapping_sub(r.oldest_unacked) < window {
                    let Some((data, more)) = r.backlog.pop_front() else { break };
                    let id = r.next_send_id;
                    r.send_buf.insert(id, OutMsg { data, more, last_sent: None });
                    r.next_send_id = id.wrapping_add(1);
                }
            }
        }
    }

    /// Build every packet that is due now. Returns encoded datagrams.
    pub fn generate_packets(&mut self, now: u64, out: &mut Vec<Vec<u8>>) {
        self.admit_backlog();
        let resend = self.resend_interval();
        loop {
            let pkt = self.build_packet(now, resend);
            match pkt {
                Some(p) => out.push(p),
                None => break,
            }
        }
    }

    fn build_packet(&mut self, now: u64, resend: u64) -> Option<Vec<u8>> {
        let budget = self.packet_budget();
        let seq = self.local_seq;
        let mut w = Writer::with_capacity(self.cfg.mtu);
        // Until we have received anything, our ack fields are meaningless; flag that so the
        // peer does not mistake them for an ack of its sequence 0.
        let kind = if self.have_received { KIND_DATA } else { KIND_DATA_NOACK };
        w.u8(kind).u64(self.token).u16(seq).u16(self.remote_seq).u32(self.recv_history);
        debug_assert_eq!(w.len(), DATA_HEADER_LEN);
        let mut carried: Vec<(u8, u16)> = Vec::new();
        let mut used = 0usize;
        let mut resent = 0u64;

        // Reliable messages first: due = never sent, or not acked within the resend interval.
        'channels: for (ci, ch) in self.channels.iter_mut().enumerate() {
            let Channel::Reliable(r) = ch else { continue };
            let mut id = r.oldest_unacked;
            while id != r.next_send_id {
                if let Some(m) = r.send_buf.get_mut(id) {
                    let due = match m.last_sent {
                        None => true,
                        Some(t) => now.saturating_sub(t) >= resend,
                    };
                    if due {
                        let need = MSG_OVERHEAD + m.data.len();
                        if used + need > budget {
                            break 'channels;
                        }
                        let mut hdr = ci as u8;
                        if m.more {
                            hdr |= MSG_FLAG_MORE_FRAGMENTS;
                        }
                        w.u8(hdr).u16(id).blob(&m.data);
                        used += need;
                        if m.last_sent.is_some() {
                            resent += 1;
                        }
                        m.last_sent = Some(now);
                        carried.push((ci as u8, id));
                    }
                }
                id = id.wrapping_add(1);
            }
        }

        // Then unreliable messages, as many as fit.
        for (ci, ch) in self.channels.iter_mut().enumerate() {
            let Channel::Unreliable { outgoing } = ch else { continue };
            while let Some(front) = outgoing.front() {
                let need = MSG_OVERHEAD + front.len();
                if used + need > budget {
                    break;
                }
                let m = outgoing.pop_front().unwrap();
                w.u8(ci as u8).blob(&m);
                used += need;
            }
        }

        let has_payload = w.len() > DATA_HEADER_LEN;
        let keepalive_due = now.saturating_sub(self.last_send) >= self.cfg.keepalive_us;
        if !has_payload && !self.ack_pending && !keepalive_due {
            return None;
        }
        self.sent.insert(seq, SentPacket { time: now, msgs: carried, acked: false });
        self.local_seq = seq.wrapping_add(1);
        self.last_send = now;
        self.ack_pending = false;
        self.stats.packets_sent += 1;
        self.stats.messages_resent += resent;
        self.stats.bytes_sent += w.len() as u64;
        Some(w.finish())
    }

    /// Process one datagram addressed to this connection; validates kind and token.
    pub fn process_packet(&mut self, data: &[u8], now: u64) -> DecodeResult<()> {
        let mut r = Reader::new(data);
        let kind = r.u8()?;
        if (kind != KIND_DATA && kind != KIND_DATA_NOACK) || r.u64()? != self.token {
            return Err(DecodeError);
        }
        let seq = r.u16()?;
        let ack = r.u16()?;
        let ack_bits = r.u32()?;
        // Parse messages fully before mutating, so malformed packets have no effect.
        let mut msgs: Vec<(u8, Option<u16>, bool, &[u8])> = Vec::new();
        while !r.is_empty() {
            let hdr = r.u8()?;
            let ch = hdr & MSG_CHANNEL_MASK;
            let more = hdr & MSG_FLAG_MORE_FRAGMENTS != 0;
            match self.cfg.channels.get(ch as usize) {
                Some(ChannelKind::ReliableOrdered) => {
                    let id = r.u16()?;
                    msgs.push((ch, Some(id), more, r.blob()?));
                }
                Some(ChannelKind::Unreliable) => msgs.push((ch, None, false, r.blob()?)),
                None => return Err(DecodeError),
            }
        }

        self.stats.packets_received += 1;
        self.stats.bytes_received += data.len() as u64;
        self.last_recv = now;
        if kind == KIND_DATA {
            self.process_acks(ack, ack_bits, now);
        }
        // Only packets carrying messages are acked (immediately, by the next generated
        // packet). Acking pure acks and keepalives too made both peers bounce empty packets
        // back and forth at their polling rate for the whole session (~1000 datagrams/s).
        let carries_messages = !msgs.is_empty();
        if self.received_seqs.contains(seq) {
            self.stats.duplicate_packets += 1;
            self.ack_pending |= carries_messages;
            return Ok(());
        }
        self.received_seqs.insert(seq, ());
        self.record_received(seq);
        self.ack_pending |= carries_messages;

        let max_msg = self.cfg.max_message_size;
        let window = self.cfg.reliable_window;
        for (ch, id, more, payload) in msgs {
            match (&mut self.channels[ch as usize], id) {
                (Channel::Unreliable { .. }, _) => self.incoming.push_back((ch, payload.to_vec())),
                (Channel::Reliable(rel), Some(id)) => {
                    if seq_less(id, rel.next_recv_id)
                        || id.wrapping_sub(rel.next_recv_id) >= window
                        || rel.recv_buf.contains(id)
                    {
                        continue;
                    }
                    rel.recv_buf.insert(id, (payload.to_vec(), more));
                    while let Some((frag, more)) = rel.recv_buf.remove(rel.next_recv_id) {
                        rel.next_recv_id = rel.next_recv_id.wrapping_add(1);
                        if rel.assembling.len() + frag.len() > max_msg {
                            rel.assembling_overflow = true;
                        }
                        if !rel.assembling_overflow {
                            if rel.assembling.is_empty() && !more {
                                self.incoming.push_back((ch, frag));
                                continue;
                            }
                            rel.assembling.extend_from_slice(&frag);
                        }
                        if !more {
                            let msg = std::mem::take(&mut rel.assembling);
                            if !std::mem::replace(&mut rel.assembling_overflow, false) {
                                self.incoming.push_back((ch, msg));
                            }
                        }
                    }
                }
                (Channel::Reliable(_), None) => unreachable!(),
            }
        }
        Ok(())
    }

    fn record_received(&mut self, seq: u16) {
        if !self.have_received {
            self.have_received = true;
            self.remote_seq = seq;
            self.recv_history = 0;
        } else if seq_greater(seq, self.remote_seq) {
            let shift = seq.wrapping_sub(self.remote_seq) as u32;
            self.recv_history = if shift > 32 {
                0
            } else {
                // Bit (shift-1) marks the previous remote_seq as received.
                let h = if shift == 32 { 0 } else { self.recv_history << shift };
                h | (1u32 << (shift - 1))
            };
            self.remote_seq = seq;
        } else {
            let back = self.remote_seq.wrapping_sub(seq) as u32;
            if (1..=32).contains(&back) {
                self.recv_history |= 1 << (back - 1);
            }
        }
    }

    fn process_acks(&mut self, ack: u16, bits: u32, now: u64) {
        self.ack_one(ack, now);
        for i in 0..32u16 {
            if bits & (1 << i) != 0 {
                self.ack_one(ack.wrapping_sub(i + 1), now);
            }
        }
    }

    fn ack_one(&mut self, seq: u16, now: u64) {
        let Some(p) = self.sent.get_mut(seq) else { return };
        if p.acked {
            return;
        }
        p.acked = true;
        let sample = now.saturating_sub(p.time) as f64;
        let msgs = std::mem::take(&mut p.msgs);
        self.stats.packets_acked += 1;
        if !self.rtt_sampled {
            self.rtt_us = sample;
            self.rtt_var_us = sample / 2.0;
            self.rtt_sampled = true;
        } else {
            let err = sample - self.rtt_us;
            self.rtt_us += 0.125 * err;
            self.rtt_var_us += 0.25 * (err.abs() - self.rtt_var_us);
        }
        for (ch, id) in msgs {
            if let Some(Channel::Reliable(r)) = self.channels.get_mut(ch as usize) {
                r.send_buf.remove(id);
                while r.oldest_unacked != r.next_send_id && !r.send_buf.contains(r.oldest_unacked)
                {
                    r.oldest_unacked = r.oldest_unacked.wrapping_add(1);
                }
            }
        }
    }
}
