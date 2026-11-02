//! Background networking thread: owns the rudp client, so packet I/O, acks and resends run
//! independently of the game's frame rate. The game thread talks to it through `Shared`.

use protocol::stream::ClientStream;
use protocol::{ClientMsg, ServerMsg, CH_RELIABLE, CH_UNRELIABLE};
use rudp::{Client, ClientState, LinkConditions};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Idle = 0,
    Connecting = 1,
    Lobby = 2,
    Started = 3,
    Disconnected = 4,
}

#[derive(Debug, Clone, Default)]
pub struct StartInfo {
    pub seed: u32,
    pub player: u8,
    pub input_delay: u16,
    pub session_id: u64,
    pub players: Vec<(u8, String)>,
    /// Session parameters from the server ("key=value" text).
    pub params: String,
}

#[derive(Default)]
pub struct State {
    pub status: Option<Status>,
    pub player: Option<u8>,
    pub start: Option<StartInfo>,
    /// Lockstep input/tick streams (exists once the session started).
    pub stream: Option<ClientStream>,
    /// Set by the game thread when it queued new input; the net thread sends promptly.
    pub input_dirty: bool,
    /// Human-readable events for the Lua side (lobby changes, kicks, departures).
    pub events: VecDeque<String>,
    pub outgoing: VecDeque<(u8, Vec<u8>)>,
    pub disconnect_reason: Option<String>,
}

pub struct Shared {
    pub state: Mutex<State>,
    /// Signalled when ticks or status change (game thread waits on it).
    pub game_cond: Condvar,
    /// Signalled when the game queued something to send (net thread waits on it).
    pub net_cond: Condvar,
    pub stop: AtomicBool,
    pub rtt_us: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State { status: Some(Status::Idle), ..Default::default() }),
            game_cond: Condvar::new(),
            net_cond: Condvar::new(),
            stop: AtomicBool::new(false),
            rtt_us: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        })
    }

    pub fn send(&self, channel: u8, msg: &ClientMsg) {
        self.state.lock().unwrap().outgoing.push_back((channel, msg.encode()));
        self.net_cond.notify_one();
    }
}

pub struct ConnectParams {
    pub addr: String,
    pub name: String,
    pub game_build: String,
    pub mod_hash: u64,
    pub netsim: Option<LinkConditions>,
}

/// Repeat unacknowledged inputs at least this often.
const STREAM_RESEND: Duration = Duration::from_millis(16);

pub fn spawn(shared: Arc<Shared>, p: ConnectParams) -> std::io::Result<std::thread::JoinHandle<()>> {
    let client = Client::connect(p.addr.as_str(), &p.name, protocol::transport_config(), p.netsim)?;
    shared.state.lock().unwrap().status = Some(Status::Connecting);
    std::thread::Builder::new().name("noita-online-net".into()).spawn(move || run(shared, client, p))
}

fn run(shared: Arc<Shared>, mut client: Client, p: ConnectParams) {
    let mut said_hello = false;
    let mut last_ping = Instant::now();
    let mut last_stream_send = Instant::now();
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            client.disconnect();
            return;
        }
        // Queue game-thread messages and, when due, the redundant input stream.
        {
            let mut s = shared.state.lock().unwrap();
            for (ch, data) in s.outgoing.drain(..) {
                let _ = client.send(ch, &data);
            }
            let dirty = std::mem::take(&mut s.input_dirty);
            if let Some(stream) = &s.stream {
                let due = dirty || (stream.has_unacked_inputs() && last_stream_send.elapsed() >= STREAM_RESEND);
                if due {
                    let _ = client.send(CH_UNRELIABLE, &stream.inputs_msg().encode());
                    last_stream_send = Instant::now();
                }
            }
        }
        client.update();

        match client.state().clone() {
            ClientState::Connected { .. } if !said_hello => {
                let hello = ClientMsg::Hello {
                    protocol: protocol::GAME_PROTOCOL_VERSION,
                    game_build: p.game_build.clone(),
                    mod_hash: p.mod_hash,
                    name: p.name.clone(),
                };
                let _ = client.send(CH_RELIABLE, &hello.encode());
                client.flush();
                said_hello = true;
            }
            ClientState::Disconnected(reason) => {
                let mut s = shared.state.lock().unwrap();
                s.status = Some(Status::Disconnected);
                s.disconnect_reason.get_or_insert_with(|| format!("{reason:?}"));
                s.events.push_back(format!("disconnected: {reason:?}"));
                drop(s);
                shared.game_cond.notify_all();
                return;
            }
            _ => {}
        }
        if said_hello && last_ping.elapsed() >= Duration::from_secs(1) {
            last_ping = Instant::now();
            let _ = client.send(CH_UNRELIABLE, &ClientMsg::Ping { t: client.now_us() }.encode());
        }

        let mut wake_game = false;
        while let Some((_, data)) = client.receive() {
            let Ok(msg) = ServerMsg::decode(&data) else { continue };
            let mut s = shared.state.lock().unwrap();
            match msg {
                ServerMsg::Welcome { player, .. } => {
                    s.player = Some(player);
                    s.status = Some(Status::Lobby);
                    s.events.push_back(format!("welcome: player {player}"));
                }
                ServerMsg::Lobby { players, needed } => {
                    s.events.push_back(format!("lobby: {}/{} {:?}", players.len(), needed, players));
                }
                ServerMsg::Start { seed, players, input_delay, session_id, params } => {
                    let player = s.player.unwrap_or(0);
                    let params = String::from_utf8_lossy(&params).into_owned();
                    s.start = Some(StartInfo { seed, player, input_delay, session_id, players: players.clone(), params });
                    s.stream = Some(ClientStream::new(input_delay as u32));
                    s.status = Some(Status::Started);
                    s.events.push_back(format!("start: seed={seed} delay={input_delay} players={players:?}"));
                    wake_game = true;
                }
                ServerMsg::Ticks { input_ack, first, ticks } => {
                    if let Some(stream) = &mut s.stream {
                        let new = stream.on_ticks(input_ack, first, ticks);
                        wake_game |= new;
                        // Tell the server promptly that we have these ticks (trims its resends).
                        if new {
                            s.input_dirty = true;
                        }
                    }
                }
                ServerMsg::PlayerLeft { player } => s.events.push_back(format!("left: {player}")),
                ServerMsg::Pong { t } => {
                    let rtt = client.now_us().saturating_sub(t);
                    shared.rtt_us.store(rtt, Ordering::Relaxed);
                }
                ServerMsg::Kick { reason } => {
                    s.events.push_back(format!("kicked: {reason}"));
                    s.disconnect_reason = Some(reason);
                }
            }
        }
        if let Some(c) = client.connection() {
            shared.bytes_sent.store(c.stats.bytes_sent, Ordering::Relaxed);
            shared.bytes_received.store(c.stats.bytes_received, Ordering::Relaxed);
        }
        if wake_game {
            shared.game_cond.notify_all();
        }
        // Sleep until the game queues something or ~1 ms passes (keeps acks/resends prompt).
        let s = shared.state.lock().unwrap();
        if s.outgoing.is_empty() && !s.input_dirty {
            let _ = shared.net_cond.wait_timeout(s, Duration::from_millis(1)).unwrap();
        }
    }
}
