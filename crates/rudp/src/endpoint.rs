//! UDP endpoints: a multi-client [`Server`] and a single-connection [`Client`], both
//! non-blocking and driven by `update(now)`. Optional [`LinkConditions`] inject loss and
//! latency on every datagram the endpoint sends *and* receives, for testing.

use crate::connection::{Config, Connection, SendError};
use crate::linksim::{LinkConditions, LinkSim};
use crate::packet::{data_token, Control, DenyReason, PROTOCOL_VERSION};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Instant;

/// Monotonic microsecond clock shared by endpoints.
#[derive(Clone, Copy)]
pub struct Clock(Instant);

impl Clock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
    pub fn now_us(&self) -> u64 {
        self.0.elapsed().as_micros() as u64
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

fn random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos() ^ (std::process::id() as u128) << 64);
    h.finish()
}

/// Socket wrapper that optionally routes traffic through a link simulator.
struct Io {
    socket: UdpSocket,
    out_sim: Option<LinkSim<SocketAddr>>,
    in_sim: Option<LinkSim<SocketAddr>>,
    recv_buf: Vec<u8>,
}

impl Io {
    fn new(socket: UdpSocket, cond: Option<LinkConditions>) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let seed = random_u64();
        Ok(Self {
            socket,
            out_sim: cond.map(|c| LinkSim::new(c, seed)),
            in_sim: cond.map(|c| LinkSim::new(c, seed ^ 0x9e37_79b9_7f4a_7c15)),
            recv_buf: vec![0u8; 65536],
        })
    }

    fn send(&mut self, data: &[u8], to: SocketAddr, now: u64) {
        match &mut self.out_sim {
            Some(sim) => sim.push(to, data.to_vec(), now),
            None => {
                let _ = self.socket.send_to(data, to);
            }
        }
    }

    fn flush(&mut self, now: u64) {
        if let Some(sim) = &mut self.out_sim {
            while let Some((to, data)) = sim.pop_due(now) {
                let _ = self.socket.send_to(&data, to);
            }
        }
    }

    /// Drain the socket (through the inbound simulator if any), handing each datagram to `f`.
    fn recv_all(&mut self, now: u64, mut f: impl FnMut(SocketAddr, &[u8])) {
        loop {
            match self.socket.recv_from(&mut self.recv_buf) {
                Ok((n, from)) => match &mut self.in_sim {
                    Some(sim) => sim.push(from, self.recv_buf[..n].to_vec(), now),
                    None => f(from, &self.recv_buf[..n]),
                },
                // Windows reports ICMP port-unreachable from earlier sends as ConnectionReset.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(_) => break,
            }
        }
        if let Some(sim) = &mut self.in_sim {
            while let Some((from, data)) = sim.pop_due(now) {
                f(from, &data);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    TimedOut,
    Requested,
    Kicked,
    Denied(DenyReason),
}

#[derive(Debug)]
pub enum ServerEvent {
    Connected { client: u16, name: String },
    Disconnected { client: u16, reason: DisconnectReason },
    Message { client: u16, channel: u8, data: Vec<u8> },
}

struct Slot {
    addr: SocketAddr,
    name: String,
    conn: Connection,
}

struct Pending {
    client_salt: u64,
    server_salt: u64,
    name: String,
    created: u64,
}

pub struct Server {
    io: Io,
    cfg: Config,
    clock: Clock,
    slots: Vec<Option<Slot>>,
    by_addr: HashMap<SocketAddr, u16>,
    pending: HashMap<SocketAddr, Pending>,
    events: VecDeque<ServerEvent>,
    out: Vec<Vec<u8>>,
}

const PENDING_TIMEOUT_US: u64 = 5_000_000;
const MAX_PENDING: usize = 256;

impl Server {
    pub fn bind(
        addr: impl ToSocketAddrs,
        max_clients: u16,
        cfg: Config,
        cond: Option<LinkConditions>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        Ok(Self {
            io: Io::new(socket, cond)?,
            cfg,
            clock: Clock::new(),
            slots: (0..max_clients).map(|_| None).collect(),
            by_addr: HashMap::new(),
            pending: HashMap::new(),
            events: VecDeque::new(),
            out: Vec::new(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.socket.local_addr()
    }

    /// The UDP socket (non-blocking), e.g. to wait for incoming datagrams.
    pub fn socket(&self) -> &UdpSocket {
        &self.io.socket
    }

    pub fn now_us(&self) -> u64 {
        self.clock.now_us()
    }

    pub fn max_clients(&self) -> u16 {
        self.slots.len() as u16
    }

    pub fn connected(&self) -> impl Iterator<Item = u16> + '_ {
        self.slots.iter().enumerate().filter(|(_, s)| s.is_some()).map(|(i, _)| i as u16)
    }

    pub fn client_name(&self, client: u16) -> Option<&str> {
        self.slots.get(client as usize)?.as_ref().map(|s| s.name.as_str())
    }

    pub fn connection(&self, client: u16) -> Option<&Connection> {
        self.slots.get(client as usize)?.as_ref().map(|s| &s.conn)
    }

    pub fn send(&mut self, client: u16, channel: u8, data: &[u8]) -> Result<(), SendError> {
        match self.slots.get_mut(client as usize).and_then(|s| s.as_mut()) {
            Some(s) => s.conn.send(channel, data),
            None => Err(SendError::NoSuchChannel),
        }
    }

    pub fn broadcast(&mut self, channel: u8, data: &[u8]) -> Result<(), SendError> {
        for s in self.slots.iter_mut().flatten() {
            s.conn.send(channel, data)?;
        }
        Ok(())
    }

    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    pub fn kick(&mut self, client: u16) {
        let now = self.now_us();
        self.drop_client(client, DisconnectReason::Kicked, now);
    }

    fn drop_client(&mut self, client: u16, reason: DisconnectReason, now: u64) {
        if let Some(slot) = self.slots.get_mut(client as usize).and_then(|s| s.take()) {
            self.by_addr.remove(&slot.addr);
            let pkt = Control::Disconnect { token: slot.conn.token() }.encode();
            for _ in 0..3 {
                self.io.send(&pkt, slot.addr, now);
            }
            self.events.push_back(ServerEvent::Disconnected { client, reason });
        }
    }

    /// Receive, process timeouts, and send everything due. Call frequently (>= 60 Hz).
    pub fn update(&mut self) {
        let now = self.now_us();
        self.receive(now);
        self.pending.retain(|_, p| now.saturating_sub(p.created) < PENDING_TIMEOUT_US);
        let timed_out: Vec<u16> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().filter(|s| s.conn.timed_out(now)).map(|_| i as u16))
            .collect();
        for c in timed_out {
            self.drop_client(c, DisconnectReason::TimedOut, now);
        }
        self.flush_now(now);
    }

    /// Send everything due without receiving (use right after queueing urgent messages).
    pub fn flush(&mut self) {
        let now = self.now_us();
        self.flush_now(now);
    }

    fn flush_now(&mut self, now: u64) {
        let mut out = std::mem::take(&mut self.out);
        for slot in self.slots.iter_mut().flatten() {
            slot.conn.generate_packets(now, &mut out);
            for p in out.drain(..) {
                self.io.send(&p, slot.addr, now);
            }
        }
        self.out = out;
        self.io.flush(now);
    }

    fn receive(&mut self, now: u64) {
        let mut datagrams: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        self.io.recv_all(now, |from, data| datagrams.push((from, data.to_vec())));
        for (from, data) in datagrams {
            self.handle_datagram(from, &data, now);
        }
    }

    fn handle_datagram(&mut self, from: SocketAddr, data: &[u8], now: u64) {
        if let Some(token) = data_token(data) {
            if let Some(&id) = self.by_addr.get(&from) {
                let slot = self.slots[id as usize].as_mut().unwrap();
                if slot.conn.token() == token && slot.conn.process_packet(data, now).is_ok() {
                    while let Some((channel, data)) = slot.conn.receive() {
                        self.events.push_back(ServerEvent::Message { client: id, channel, data });
                    }
                }
            }
            return;
        }
        let Ok(Some(ctrl)) = Control::decode(data) else { return };
        match ctrl {
            Control::ConnectRequest { version, client_salt, name } => {
                if self.by_addr.contains_key(&from) {
                    // Stale retry from an already-connected address; the accept is re-sent
                    // on ChallengeResponse retries instead.
                    return;
                }
                if version != PROTOCOL_VERSION {
                    let pkt = Control::Denied { client_salt, reason: DenyReason::VersionMismatch };
                    self.io.send(&pkt.encode(), from, now);
                    return;
                }
                if self.slots.iter().all(|s| s.is_some()) {
                    let pkt = Control::Denied { client_salt, reason: DenyReason::ServerFull };
                    self.io.send(&pkt.encode(), from, now);
                    return;
                }
                if !self.pending.contains_key(&from) && self.pending.len() >= MAX_PENDING {
                    return;
                }
                let p = self.pending.entry(from).or_insert_with(|| Pending {
                    client_salt,
                    server_salt: random_u64(),
                    name: name.clone(),
                    created: now,
                });
                if p.client_salt != client_salt {
                    *p = Pending { client_salt, server_salt: random_u64(), name, created: now };
                }
                let pkt = Control::Challenge { client_salt, server_salt: p.server_salt };
                self.io.send(&pkt.encode(), from, now);
            }
            Control::ChallengeResponse { token } => {
                if let Some(&id) = self.by_addr.get(&from) {
                    let slot = self.slots[id as usize].as_ref().unwrap();
                    if slot.conn.token() == token {
                        let pkt = Control::Accepted {
                            token,
                            client_id: id,
                            max_clients: self.max_clients(),
                        };
                        self.io.send(&pkt.encode(), from, now);
                    }
                    return;
                }
                let Some(p) = self.pending.get(&from) else { return };
                if p.client_salt ^ p.server_salt != token {
                    return;
                }
                let Some(id) = self.slots.iter().position(|s| s.is_none()) else {
                    let pkt = Control::Denied {
                        client_salt: p.client_salt,
                        reason: DenyReason::ServerFull,
                    };
                    self.io.send(&pkt.encode(), from, now);
                    return;
                };
                let p = self.pending.remove(&from).unwrap();
                let id = id as u16;
                self.slots[id as usize] = Some(Slot {
                    addr: from,
                    name: p.name.clone(),
                    conn: Connection::new(self.cfg.clone(), token, now),
                });
                self.by_addr.insert(from, id);
                let pkt = Control::Accepted { token, client_id: id, max_clients: self.max_clients() };
                self.io.send(&pkt.encode(), from, now);
                self.events.push_back(ServerEvent::Connected { client: id, name: p.name });
            }
            Control::Disconnect { token } => {
                if let Some(&id) = self.by_addr.get(&from) {
                    if self.slots[id as usize].as_ref().unwrap().conn.token() == token {
                        self.slots[id as usize] = None;
                        self.by_addr.remove(&from);
                        self.events.push_back(ServerEvent::Disconnected {
                            client: id,
                            reason: DisconnectReason::Requested,
                        });
                    }
                }
            }
            Control::Challenge { .. } | Control::Accepted { .. } | Control::Denied { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    Connecting,
    Connected { client_id: u16, max_clients: u16 },
    Disconnected(DisconnectReason),
}

pub struct Client {
    io: Io,
    cfg: Config,
    clock: Clock,
    server: SocketAddr,
    name: String,
    client_salt: u64,
    server_salt: Option<u64>,
    last_handshake_send: Option<u64>,
    started: u64,
    state: ClientState,
    conn: Option<Connection>,
    incoming: VecDeque<(u8, Vec<u8>)>,
    out: Vec<Vec<u8>>,
}

const HANDSHAKE_RESEND_US: u64 = 250_000;

impl Client {
    pub fn connect(
        server: impl ToSocketAddrs,
        name: &str,
        cfg: Config,
        cond: Option<LinkConditions>,
    ) -> io::Result<Self> {
        let server = server
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))?;
        let bind: SocketAddr =
            if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        let socket = UdpSocket::bind(bind)?;
        let clock = Clock::new();
        Ok(Self {
            io: Io::new(socket, cond)?,
            cfg,
            clock,
            server,
            name: name.to_owned(),
            client_salt: random_u64(),
            server_salt: None,
            last_handshake_send: None,
            started: clock.now_us(),
            state: ClientState::Connecting,
            conn: None,
            incoming: VecDeque::new(),
            out: Vec::new(),
        })
    }

    pub fn state(&self) -> &ClientState {
        &self.state
    }

    pub fn now_us(&self) -> u64 {
        self.clock.now_us()
    }

    /// The UDP socket (non-blocking), e.g. to wait for incoming datagrams.
    pub fn socket(&self) -> &UdpSocket {
        &self.io.socket
    }

    /// Earliest time ([`Self::now_us`] clock) at which [`Self::update`] has timed work to do
    /// without new input or datagrams (handshake resend, reliable resend, ack, keepalive,
    /// timeout check). `None` while a link simulator holds datagrams for later: poll then.
    pub fn next_deadline_us(&self) -> Option<u64> {
        self.deadline(false)
    }

    /// [`Self::next_deadline_us`] for a caller whose own next packet (soon) carries the
    /// pending acks.
    pub fn next_deadline_us_deferring_acks(&self) -> Option<u64> {
        self.deadline(true)
    }

    fn deadline(&self, defer_acks: bool) -> Option<u64> {
        if self.io.in_sim.is_some() || self.io.out_sim.is_some() {
            return None;
        }
        let now = self.now_us();
        Some(match &self.state {
            ClientState::Connecting => self
                .last_handshake_send
                .map_or(now, |t| t + HANDSHAKE_RESEND_US)
                .min(self.started + self.cfg.timeout_us + 1),
            ClientState::Connected { .. } => self.conn.as_ref().map_or(now, |c| {
                if defer_acks {
                    c.next_deadline_deferring_acks(now)
                } else {
                    c.next_deadline(now)
                }
            }),
            ClientState::Disconnected(_) => u64::MAX,
        })
    }

    pub fn connection(&self) -> Option<&Connection> {
        self.conn.as_ref()
    }

    pub fn send(&mut self, channel: u8, data: &[u8]) -> Result<(), SendError> {
        match &mut self.conn {
            Some(c) => c.send(channel, data),
            None => Err(SendError::NoSuchChannel),
        }
    }

    pub fn receive(&mut self) -> Option<(u8, Vec<u8>)> {
        self.incoming.pop_front()
    }

    pub fn disconnect(&mut self) {
        let now = self.now_us();
        if let Some(c) = &self.conn {
            let pkt = Control::Disconnect { token: c.token() }.encode();
            for _ in 0..3 {
                self.io.send(&pkt, self.server, now);
            }
        }
        self.io.flush(now + 10_000_000);
        self.conn = None;
        self.state = ClientState::Disconnected(DisconnectReason::Requested);
    }

    pub fn update(&mut self) {
        let now = self.now_us();
        self.receive_now(now);
        match self.state {
            ClientState::Connecting => {
                if now.saturating_sub(self.started) > self.cfg.timeout_us {
                    self.state = ClientState::Disconnected(DisconnectReason::TimedOut);
                } else if self
                    .last_handshake_send
                    .map_or(true, |t| now.saturating_sub(t) >= HANDSHAKE_RESEND_US)
                {
                    let pkt = match self.server_salt {
                        None => Control::ConnectRequest {
                            version: PROTOCOL_VERSION,
                            client_salt: self.client_salt,
                            name: self.name.clone(),
                        },
                        Some(s) => Control::ChallengeResponse { token: self.client_salt ^ s },
                    };
                    self.io.send(&pkt.encode(), self.server, now);
                    self.last_handshake_send = Some(now);
                }
            }
            ClientState::Connected { .. } => {
                let conn = self.conn.as_mut().unwrap();
                if conn.timed_out(now) {
                    self.conn = None;
                    self.state = ClientState::Disconnected(DisconnectReason::TimedOut);
                }
            }
            ClientState::Disconnected(_) => {}
        }
        self.flush_now(now);
    }

    /// Read the datagrams that arrived (like [`Self::update`], but sends nothing), so the
    /// caller can answer them in the same packet as its own messages.
    pub fn poll(&mut self) {
        let now = self.now_us();
        self.receive_now(now);
    }

    pub fn flush(&mut self) {
        let now = self.now_us();
        self.flush_now(now);
    }

    fn flush_now(&mut self, now: u64) {
        if let Some(conn) = &mut self.conn {
            conn.generate_packets(now, &mut self.out);
            for p in self.out.drain(..) {
                self.io.send(&p, self.server, now);
            }
        }
        self.io.flush(now);
    }

    fn receive_now(&mut self, now: u64) {
        let server = self.server;
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        self.io.recv_all(now, |from, data| {
            if from == server {
                datagrams.push(data.to_vec());
            }
        });
        for data in datagrams {
            if let Some(token) = data_token(&data) {
                if let Some(conn) = &mut self.conn {
                    if conn.token() == token && conn.process_packet(&data, now).is_ok() {
                        while let Some(m) = conn.receive() {
                            self.incoming.push_back(m);
                        }
                    }
                }
                continue;
            }
            let Ok(Some(ctrl)) = Control::decode(&data) else { continue };
            match ctrl {
                Control::Challenge { client_salt, server_salt } => {
                    if self.state == ClientState::Connecting && client_salt == self.client_salt {
                        if self.server_salt != Some(server_salt) {
                            self.server_salt = Some(server_salt);
                            self.last_handshake_send = None;
                        }
                    }
                }
                Control::Accepted { token, client_id, max_clients } => {
                    let expected = self.server_salt.map(|s| s ^ self.client_salt);
                    if self.state == ClientState::Connecting && Some(token) == expected {
                        self.conn = Some(Connection::new(self.cfg.clone(), token, now));
                        self.state = ClientState::Connected { client_id, max_clients };
                    }
                }
                Control::Denied { client_salt, reason } => {
                    if self.state == ClientState::Connecting && client_salt == self.client_salt {
                        self.state = ClientState::Disconnected(DisconnectReason::Denied(reason));
                    }
                }
                Control::Disconnect { token } => {
                    if self.conn.as_ref().is_some_and(|c| c.token() == token) {
                        self.conn = None;
                        self.state = ClientState::Disconnected(DisconnectReason::Kicked);
                    }
                }
                Control::ConnectRequest { .. } | Control::ChallengeResponse { .. } => {}
            }
        }
    }
}
