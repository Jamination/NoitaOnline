//! Background networking thread: owns the rudp client, so packet I/O, acks and resends run
//! independently of the game's frame rate. The game thread talks to it through `Shared`.

use protocol::stream::ClientStream;
use protocol::{ClientMsg, ServerMsg, CH_RELIABLE, CH_UNRELIABLE};
use rudp::{Client, ClientState, LinkConditions};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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
    /// The game thread is blocked waiting for a tick: the net thread watches the socket.
    pub game_waiting: bool,
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
    pub net_wake: NetWake,
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
            net_wake: NetWake::new(),
            stop: AtomicBool::new(false),
            rtt_us: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        })
    }

    /// Queue a message. Before the session it goes out at once; during the session it goes
    /// out with the next input (`no_send_input`), so the network thread wakes once per frame.
    pub fn send(&self, channel: u8, msg: &ClientMsg) {
        let mut s = self.state.lock().unwrap();
        s.outgoing.push_back((channel, msg.encode()));
        let batched = s.stream.is_some();
        drop(s);
        if !batched {
            self.net_wake.notify();
        }
    }
}

/// Wakes the network thread. On Windows the thread also wakes when a datagram arrives, so it
/// sleeps until there is work instead of polling the socket every millisecond (each poll is a
/// few syscalls under Wine, on the core the game runs on).
pub struct NetWake {
    cond: Condvar,
    #[cfg(windows)]
    event: Option<win::Event>,
}

impl NetWake {
    fn new() -> Self {
        Self {
            cond: Condvar::new(),
            #[cfg(windows)]
            event: win::Event::new(),
        }
    }

    pub fn notify(&self) {
        #[cfg(windows)]
        if let Some(e) = &self.event {
            e.set();
        }
        self.cond.notify_all();
    }

    /// Release `state` and sleep until [`Self::notify`], `timeout`, or (with `watch_socket`)
    /// a datagram on the socket that `socket_event` watches. Without the events, wakes at
    /// least every 1 ms while watching the socket.
    fn wait(&self, state: MutexGuard<'_, State>, socket_event: Option<&SocketEvent>, watch_socket: bool, timeout: Duration) {
        #[cfg(windows)]
        if let (Some(wake), Some(sock)) = (&self.event, socket_event) {
            drop(state);
            if watch_socket {
                win::wait_any(&[wake, sock], timeout);
            } else {
                win::wait_any(&[wake], timeout);
            }
            return;
        }
        let _ = socket_event;
        let poll = if watch_socket { Duration::from_millis(1) } else { Duration::MAX };
        let _ = self.cond.wait_timeout(state, timeout.min(poll)).unwrap();
    }
}

#[cfg(windows)]
type SocketEvent = win::Event;
#[cfg(not(windows))]
type SocketEvent = ();

/// An event set whenever the client socket has datagrams to read (None: poll instead).
fn socket_event(client: &Client) -> Option<SocketEvent> {
    #[cfg(windows)]
    {
        let e = win::Event::new()?;
        e.on_readable(client.socket()).then_some(e)
    }
    #[cfg(not(windows))]
    {
        let _ = client;
        None
    }
}

#[cfg(windows)]
mod win {
    use std::time::Duration;

    type Handle = isize;
    extern "system" {
        fn CreateEventW(attrs: *mut u8, manual_reset: i32, initial_state: i32, name: *const u16) -> Handle;
        fn SetEvent(event: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn WaitForMultipleObjects(count: u32, handles: *const Handle, wait_all: i32, ms: u32) -> u32;
    }
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAEventSelect(socket: usize, event: Handle, network_events: i32) -> i32;
    }
    const FD_READ: i32 = 1;

    /// Auto-reset event.
    pub struct Event(Handle);

    impl Event {
        pub fn new() -> Option<Self> {
            let h = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
            (h != 0).then_some(Self(h))
        }

        pub fn set(&self) {
            unsafe { SetEvent(self.0) };
        }

        /// Set this event whenever `socket` has datagrams to read (re-armed by every receive
        /// that finds the socket empty).
        pub fn on_readable(&self, socket: &std::net::UdpSocket) -> bool {
            use std::os::windows::io::AsRawSocket;
            unsafe { WSAEventSelect(socket.as_raw_socket() as usize, self.0, FD_READ) == 0 }
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Wait until any of `events` (at most 2) is set or `timeout` passes (rounded up to whole ms).
    pub fn wait_any(events: &[&Event], timeout: Duration) {
        let mut handles = [0 as Handle; 2];
        for (h, e) in handles.iter_mut().zip(events) {
            *h = e.0;
        }
        let ms = timeout.as_micros().div_ceil(1000).min(u32::MAX as u128 - 1) as u32;
        unsafe { WaitForMultipleObjects(events.len().min(2) as u32, handles.as_ptr(), 0, ms) };
    }
}

pub struct ConnectParams {
    pub addr: String,
    pub name: String,
    pub game_build: String,
    pub mod_hash: u64,
    pub netsim: Option<LinkConditions>,
}

/// Repeat unacknowledged inputs at least this often while the game waits for a tick (else
/// every input goes out again with the next one).
const STREAM_RESEND: Duration = Duration::from_millis(16);
/// ... and this often while the game runs: a frame longer than this (a stall) still repeats
/// them.
const STREAM_RESEND_RUNNING: Duration = Duration::from_millis(100);
/// Resend floor of the client's reliable messages. They (state reports, logs) are never
/// urgent, and the acks of the frame's report are only read with the next frame's input, so
/// the rudp default (20 ms) would resend every report at frame rates below 50.
const RELIABLE_RESEND_MIN_US: u64 = 100_000;
/// Round-trip probe interval.
const PING_EVERY: Duration = Duration::from_secs(1);
/// Upper bound on one sleep of the network thread (all timed work is computed exactly).
const MAX_SLEEP: Duration = Duration::from_millis(250);

pub fn spawn(shared: Arc<Shared>, p: ConnectParams) -> std::io::Result<std::thread::JoinHandle<()>> {
    let cfg = rudp::Config { resend_min_us: RELIABLE_RESEND_MIN_US, ..protocol::transport_config() };
    let client = Client::connect(p.addr.as_str(), &p.name, cfg, p.netsim)?;
    shared.state.lock().unwrap().status = Some(Status::Connecting);
    std::thread::Builder::new().name("noita-online-net".into()).spawn(move || run(shared, client, p))
}

/// During the session the thread wakes once per frame, when the game queues its input: it
/// reads what arrived (ticks, acks), then sends one packet with the input stream, the tick
/// and packet acks and the queued messages (the last frame's state report). Datagrams wake
/// it only while the game waits for a tick (or before the session).
fn run(shared: Arc<Shared>, mut client: Client, p: ConnectParams) {
    let mut said_hello = false;
    let mut last_ping = Instant::now();
    let mut last_stream_send = Instant::now();
    let sock_event = socket_event(&client);
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            // The messages waiting for the next input (the last state reports) go out first.
            for (ch, data) in shared.state.lock().unwrap().outgoing.drain(..) {
                let _ = client.send(ch, &data);
            }
            client.flush();
            client.disconnect();
            return;
        }
        // Datagrams first, so this round's packet carries their acks and the newest tick ack.
        client.poll();
        let mut wake_game = receive(&shared, &mut client);
        // Queue game-thread messages and, when due, the redundant input stream.
        {
            let mut s = shared.state.lock().unwrap();
            for (ch, data) in s.outgoing.drain(..) {
                let _ = client.send(ch, &data);
            }
            let dirty = std::mem::take(&mut s.input_dirty);
            let resend = if s.game_waiting { STREAM_RESEND } else { STREAM_RESEND_RUNNING };
            if let Some(stream) = &s.stream {
                let due = dirty || (stream.has_unacked_inputs() && last_stream_send.elapsed() >= resend);
                if due {
                    let _ = client.send(CH_UNRELIABLE, &stream.inputs_msg().encode());
                    last_stream_send = Instant::now();
                }
            }
        }
        if said_hello && last_ping.elapsed() >= PING_EVERY {
            last_ping = Instant::now();
            let _ = client.send(CH_UNRELIABLE, &ClientMsg::Ping { t: client.now_us() }.encode());
        }
        client.update();
        wake_game |= receive(&shared, &mut client);

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
                last_ping = Instant::now();
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
        if let Some(c) = client.connection() {
            shared.bytes_sent.store(c.stats.bytes_sent, Ordering::Relaxed);
            shared.bytes_received.store(c.stats.bytes_received, Ordering::Relaxed);
        }
        if wake_game {
            shared.game_cond.notify_all();
        }
        // Sleep until the game queues its input (or, before the session, any message), a
        // datagram arrives while the socket is watched, or timed work is due (input stream
        // resend, ping, rudp ack/resend/keepalive).
        let s = shared.state.lock().unwrap();
        if !s.input_dirty && (s.stream.is_some() || s.outgoing.is_empty()) {
            let now = Instant::now();
            let mut until = now + MAX_SLEEP;
            if said_hello && s.stream.is_none() {
                // (During the session the probe goes out with the next input.)
                until = until.min(last_ping + PING_EVERY);
            }
            if s.stream.as_ref().is_some_and(|st| st.has_unacked_inputs()) {
                until = until.min(last_stream_send + if s.game_waiting { STREAM_RESEND } else { STREAM_RESEND_RUNNING });
            }
            until = until.min(match client.next_deadline_us() {
                Some(at) => now + Duration::from_micros(at.saturating_sub(client.now_us())),
                None => now + Duration::from_millis(1),
            });
            let watch_socket = s.stream.is_none() || s.game_waiting;
            shared.net_wake.wait(s, sock_event.as_ref(), watch_socket, until.saturating_duration_since(now));
        }
    }
}

/// Handle the server messages the client has received. Returns whether the game thread
/// should wake (session start, new ticks).
fn receive(shared: &Shared, client: &mut Client) -> bool {
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
                // The tick ack goes out with the next input.
                if let Some(stream) = &mut s.stream {
                    wake_game |= stream.on_ticks(input_ack, first, ticks);
                }
            }
            ServerMsg::PlayerLeft { player } => s.events.push_back(format!("left: {player}")),
            // t = 0: the server's round-trip probe in the lobby, not an answer to our ping.
            ServerMsg::Pong { t } if t != 0 => {
                let rtt = client.now_us().saturating_sub(t);
                shared.rtt_us.store(rtt, Ordering::Relaxed);
            }
            ServerMsg::Pong { .. } => {}
            ServerMsg::Kick { reason } => {
                s.events.push_back(format!("kicked: {reason}"));
                s.disconnect_reason = Some(reason);
            }
        }
    }
    wake_game
}
