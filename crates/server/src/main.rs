//! Noita Online headless server.
//!
//! Hosts one lockstep session: clients connect over reliable UDP, the session starts once
//! `--players` clients have joined, then the server relays every player's per-frame input
//! as one ordered `Tick` per simulation frame and cross-checks clients' state hashes.
//!
//! A session is a series of runs (worlds). When every player has died at once the game is
//! over: each client starts a new game and says `NextRun`; the server then drops it from the
//! old run's ticks (like a departed player: the clients still on the game-over screen keep
//! simulating the old world in lockstep), and once every client is ready it starts the next
//! run with a new seed (`Start`, same roster), frames counting from 0 again.

mod delay;
mod lockstep;
mod metrics;
mod verify;

use protocol::{ClientMsg, ServerMsg, CH_RELIABLE, CH_UNRELIABLE, GAME_PROTOCOL_VERSION};
use rudp::{LinkConditions, Server, ServerEvent};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Args {
    bind: String,
    players: u8,
    max_players: u8,
    seed: Option<u32>,
    /// Fixed lockstep input delay in frames; None = derive it from measured RTTs at start
    /// and adapt it to the clients' tick margins during the session (`delay`).
    input_delay: Option<u16>,
    netsim: Option<LinkConditions>,
    log_dir: Option<PathBuf>,
    exit_when_empty: bool,
    game_build: Option<String>,
    /// Session scenario name handed to every client (e.g. "bot" = scripted test session).
    scenario: String,
    /// Share of a respawning player's gold lost (0..=1, rounded down to whole gold).
    respawn_gold_loss: f32,
    /// Number of inventory items (wands, flasks, other items) a respawning player loses.
    respawn_item_loss: u32,
}

const USAGE: &str = "noita-online-server [--bind ADDR:PORT] [--players N] [--max-players N] [--seed N]
    [--input-delay FRAMES] [--netsim loss=0.05,latency=100,jitter=0] [--log-dir DIR]
    [--exit-when-empty] [--game-build STRING] [--scenario NAME]
    [--respawn-gold-loss FRACTION] [--respawn-item-loss N]";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        bind: "0.0.0.0:41773".into(),
        players: 2,
        max_players: 8,
        seed: None,
        input_delay: None,
        netsim: None,
        log_dir: None,
        exit_when_empty: false,
        game_build: None,
        scenario: String::new(),
        respawn_gold_loss: 0.5,
        respawn_item_loss: 1,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--bind" => a.bind = v()?,
            "--players" => a.players = v()?.parse().map_err(|_| "bad --players")?,
            "--max-players" => a.max_players = v()?.parse().map_err(|_| "bad --max-players")?,
            "--seed" => {
                let s: u32 = v()?.parse().map_err(|_| "bad --seed")?;
                if s == 0 || s > i32::MAX as u32 {
                    return Err("--seed must be in 1..=2147483647".into());
                }
                a.seed = Some(s);
            }
            "--input-delay" => {
                let s = v()?;
                a.input_delay = if s == "auto" { None } else { Some(s.parse().map_err(|_| "bad --input-delay")?) };
            }
            "--netsim" => a.netsim = Some(LinkConditions::parse(&v()?)?),
            "--log-dir" => a.log_dir = Some(PathBuf::from(v()?)),
            "--exit-when-empty" => a.exit_when_empty = true,
            "--game-build" => a.game_build = Some(v()?),
            "--scenario" => a.scenario = v()?,
            "--respawn-gold-loss" => {
                let f: f32 = v()?.parse().map_err(|_| "bad --respawn-gold-loss")?;
                if !(0.0..=1.0).contains(&f) {
                    return Err("--respawn-gold-loss must be in 0..=1".into());
                }
                a.respawn_gold_loss = f;
            }
            "--respawn-item-loss" => a.respawn_item_loss = v()?.parse().map_err(|_| "bad --respawn-item-loss")?,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {k}\n{USAGE}")),
        }
    }
    if a.players == 0 || a.players > a.max_players {
        return Err("--players must be in 1..=max-players".into());
    }
    Ok(a)
}

struct Player {
    name: String,
    hello: bool,
    bytes_out: metrics::Rate,
    bytes_in: metrics::Rate,
    /// Next tick frame this client still needs.
    tick_ack: u32,
    /// Input ack last sent to this client (resend promptly when it advances).
    sent_input_ack: u32,
    last_stream_send: Instant,
    /// The client plays the current run (false: in the lobby, or it left the run and waits
    /// for the next one).
    in_run: bool,
}

/// While a client lacks ticks (or an input ack is news), repeat the stream this often. Every
/// new tick already repeats all the ticks a client lacks, and clients acknowledge ticks only
/// with their next input (once per frame), so this only matters when the stream pauses; a
/// shorter interval would resend every frame to clients below 60 fps.
const STREAM_RESEND: Duration = Duration::from_millis(50);

#[derive(Default)]
struct PerfAgg {
    reports: u64,
    frames: u64,
    sum_avg_us: f64,
    worst_p99_us: u32,
    stalls: u64,
}

struct Logs {
    dir: Option<PathBuf>,
    net: Option<std::fs::File>,
    perf: Option<std::fs::File>,
    events: Option<std::fs::File>,
}

impl Logs {
    fn open(dir: Option<PathBuf>) -> std::io::Result<Self> {
        let f = |name: &str| -> std::io::Result<Option<std::fs::File>> {
            match &dir {
                Some(d) => Ok(Some(std::fs::File::create(d.join(name))?)),
                None => Ok(None),
            }
        };
        if let Some(d) = &dir {
            std::fs::create_dir_all(d)?;
        }
        Ok(Self { net: f("net.jsonl")?, perf: f("perf.jsonl")?, events: f("events.log")?, dir })
    }

    fn event(&mut self, msg: &str) {
        let line = format!("[{:>10.3}] {msg}", elapsed_secs());
        println!("{line}");
        if let Some(f) = &mut self.events {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn elapsed_secs() -> f64 {
    use std::sync::LazyLock;
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_secs_f64()
}

enum Phase {
    Lobby,
    Running { relay: lockstep::Relay, delay: delay::Controller, started: Instant, session_id: u64, seed: u32, run: u32 },
}

/// A finished run, for the summary.
#[derive(Clone, Copy)]
struct RunLog {
    run: u32,
    seed: u32,
    frames: u32,
    seconds: f64,
}

/// The world seed of run `run`: `--seed` itself for the first run and, for later runs, one
/// derived from it (reproducible tests); random without `--seed`. Noita parses WORLD_SEED as
/// a signed 32-bit int: 1..=i32::MAX.
fn run_seed(fixed: Option<u32>, run: u32) -> u32 {
    let v = match fixed {
        Some(s) if run == 0 => return s,
        Some(s) => {
            // splitmix64 of (seed, run)
            let mut x = ((s as u64) << 32 | run as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            x ^ (x >> 31)
        }
        None => rudp_random(),
    };
    (v % i32::MAX as u64) as u32 + 1
}

/// The largest round trip among `ids` (each link's fastest sample once probed), for the
/// input delay.
fn max_rtt_us(server: &Server, ids: &[u8]) -> u64 {
    ids.iter()
        .filter_map(|i| server.connection(*i as u16))
        .map(|c| if c.rtt_samples() > 0 { c.rtt_min_us() } else { c.rtt_us() })
        .max()
        .unwrap_or(0)
}

/// Start run `run` for `ids` (every client that said hello): broadcast `Start` and return the
/// running phase. `delay`: the run's input delay controller (its current delay is the start
/// delay).
#[allow(clippy::too_many_arguments)]
fn start_run(
    server: &mut Server,
    logs: &mut Logs,
    args: &Args,
    players: &mut BTreeMap<u8, Player>,
    ids: Vec<u8>,
    run: u32,
    session_id: u64,
    delay: delay::Controller,
    max_rtt: u64,
) -> Phase {
    let seed = run_seed(args.seed, run);
    let input_delay = delay.delay();
    let roster: Vec<(u8, String)> = ids.iter().map(|i| (*i, players[i].name.clone())).collect();
    // Shared calendar for date/time-dependent content (weather, holidays).
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let params = format!(
        "scenario={};time={};respawn_gold_loss={};respawn_item_loss={};run={run}",
        args.scenario, now, args.respawn_gold_loss, args.respawn_item_loss
    )
    .into_bytes();
    let start = ServerMsg::Start { run, seed, players: roster.clone(), input_delay, session_id, params };
    let _ = server.broadcast(CH_RELIABLE, &start.encode());
    server.flush();
    for id in &ids {
        if let Some(p) = players.get_mut(id) {
            p.in_run = true;
            p.tick_ack = 0;
            p.sent_input_ack = 0;
        }
    }
    logs.event(&format!(
        "session {session_id:016x} run {run} started: seed={seed} input_delay={input_delay} (max rtt {max_rtt} us) players={roster:?}"
    ));
    Phase::Running {
        relay: lockstep::Relay::new(ids, input_delay as u32),
        delay,
        started: Instant::now(),
        session_id,
        seed,
        run,
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> std::io::Result<()> {
    elapsed_secs();
    let mut logs = Logs::open(args.log_dir.clone())?;
    let hashes_file = match &logs.dir {
        Some(d) => Some(std::fs::File::create(d.join("hashes.jsonl"))?),
        None => None,
    };
    let mut verifier = verify::Verifier::new(hashes_file);
    let mut server = Server::bind(&args.bind, args.max_players as u16, protocol::transport_config(), args.netsim)?;
    logs.event(&format!(
        "listening on {} (players={}, max={}, input_delay={:?}, netsim={:?})",
        server.local_addr()?,
        args.players,
        args.max_players,
        args.input_delay,
        args.netsim
    ));

    let mut players: BTreeMap<u8, Player> = BTreeMap::new();
    let mut departed: Vec<(u8, Player)> = Vec::new();
    let mut phase = Phase::Lobby;
    let mut runs: Vec<RunLog> = Vec::new();
    let mut perf: BTreeMap<u8, PerfAgg> = BTreeMap::new();
    let cpu_start = metrics::process_cpu_time();
    let wall_start = Instant::now();
    let mut last_metrics = Instant::now();
    let mut cpu_last = cpu_start;
    let mut cpu_peak = 0.0f64;
    let mut ticks_sent: u64 = 0;
    let mut delay_changes: u32 = 0;
    let mut had_session = false;
    let mut expected_mods: Option<u64> = None;
    // Lobby round-trip measurement before a start: (since, each ready client's sample count then).
    let mut rtt_settle: Option<(Instant, BTreeMap<u8, u32>)> = None;
    let mut last_rtt_probe = Instant::now();

    loop {
        server.update();
        while let Some(ev) = server.poll_event() {
            match ev {
                ServerEvent::Connected { client, name } => {
                    let id = client as u8;
                    logs.event(&format!("client {id} connected ({name})"));
                    if matches!(phase, Phase::Running { .. }) {
                        let _ = server.send(client, CH_RELIABLE, &ServerMsg::Kick { reason: "session in progress".into() }.encode());
                        server.flush();
                        server.kick(client);
                        continue;
                    }
                    players.insert(
                        id,
                        Player {
                            name,
                            hello: false,
                            bytes_out: Default::default(),
                            bytes_in: Default::default(),
                            tick_ack: 0,
                            sent_input_ack: 0,
                            last_stream_send: Instant::now(),
                            in_run: false,
                        },
                    );
                }
                ServerEvent::Disconnected { client, reason } => {
                    let id = client as u8;
                    if let Some(p) = players.remove(&id) {
                        departed.push((id, p));
                        logs.event(&format!("client {id} disconnected: {reason:?}"));
                        if let Phase::Running { relay, .. } = &mut phase {
                            relay.remove(id);
                            let _ = server.broadcast(CH_RELIABLE, &ServerMsg::PlayerLeft { player: id }.encode());
                        }
                    }
                }
                ServerEvent::Message { client, channel: _, data } => {
                    let id = client as u8;
                    let msg = match ClientMsg::decode(&data) {
                        Ok(m) => m,
                        Err(_) => {
                            logs.event(&format!("client {id}: malformed message, kicking"));
                            server.kick(client);
                            players.remove(&id);
                            continue;
                        }
                    };
                    match msg {
                        ClientMsg::Hello { protocol, game_build, mod_hash, name } => {
                            let build_ok = args.game_build.as_ref().is_none_or(|b| *b == game_build);
                            // Every client must run the same mod set (mods change the simulation):
                            // the first client to say hello defines it for the session.
                            let mods_ok = *expected_mods.get_or_insert(mod_hash) == mod_hash;
                            if !mods_ok {
                                let reason = format!("mod list differs from the session's ({mod_hash:016x})");
                                logs.event(&format!("client {id}: {reason}"));
                                let _ = server.send(client, CH_RELIABLE, &ServerMsg::Kick { reason }.encode());
                                server.flush();
                                server.kick(client);
                                players.remove(&id);
                                continue;
                            }
                            if protocol != GAME_PROTOCOL_VERSION || !build_ok {
                                let reason = format!("version mismatch (protocol {protocol}, build {game_build})");
                                logs.event(&format!("client {id}: {reason}"));
                                let _ = server.send(client, CH_RELIABLE, &ServerMsg::Kick { reason }.encode());
                                server.flush();
                                server.kick(client);
                                players.remove(&id);
                                continue;
                            }
                            if let Some(p) = players.get_mut(&id) {
                                p.hello = true;
                                p.name = name.clone();
                            }
                            logs.event(&format!("client {id} hello: name={name} build={game_build} mod={mod_hash:016x}"));
                            let _ = server.send(client, CH_RELIABLE, &ServerMsg::Welcome { player: id, max_players: args.max_players }.encode());
                            let roster: Vec<(u8, String)> =
                                players.iter().filter(|(_, p)| p.hello).map(|(i, p)| (*i, p.name.clone())).collect();
                            let _ = server.broadcast(CH_RELIABLE, &ServerMsg::Lobby { players: roster, needed: args.players }.encode());
                        }
                        ClientMsg::Inputs { run, tick_ack, first, inputs } => {
                            // A late datagram of an earlier run, or one sent before the server
                            // saw the client leave its run, is ignored.
                            let Phase::Running { relay, run: current, .. } = &mut phase else { continue };
                            let Some(p) = players.get_mut(&id) else { continue };
                            if run != *current || !p.in_run {
                                continue;
                            }
                            for (i, data) in inputs.iter().enumerate() {
                                relay.submit(id, first + i as u32, data);
                            }
                            p.tick_ack = p.tick_ack.max(tick_ack);
                        }
                        ClientMsg::StateHash { frame, entities, chunks, positions } => {
                            // Reliable and ordered: a client's reports before its NextRun are
                            // its run's, after the next Start the next run's.
                            if let Phase::Running { relay, run, .. } = &phase {
                                if !players.get(&id).is_some_and(|p| p.in_run) {
                                    continue;
                                }
                                let active = relay.active().to_vec();
                                for o in verifier.add(*run, frame, id, verify::Report { entities, chunks, positions }, &active) {
                                    if o.chunk_mismatches > 0 || o.position_mismatches > 0 {
                                        logs.event(&format!("MISMATCH {o:?}"));
                                    }
                                }
                            }
                        }
                        ClientMsg::Perf { frames, avg_frame_us, p99_frame_us, max_frame_us, wait_us, stalls, margin_min_us, rtt_us } => {
                            let a = perf.entry(id).or_default();
                            a.reports += 1;
                            a.frames += frames as u64;
                            a.sum_avg_us += avg_frame_us as f64 * frames as f64;
                            a.worst_p99_us = a.worst_p99_us.max(p99_frame_us);
                            a.stalls += stalls as u64;
                            if let Some(f) = &mut logs.perf {
                                let _ = writeln!(
                                    f,
                                    "{{\"t\":{:.3},\"player\":{id},\"frames\":{frames},\"avg_frame_us\":{avg_frame_us},\"p99_frame_us\":{p99_frame_us},\"max_frame_us\":{max_frame_us},\"wait_us\":{wait_us},\"stalls\":{stalls},\"margin_min_us\":{},\"rtt_us\":{rtt_us}}}",
                                    elapsed_secs(),
                                    if margin_min_us == u32::MAX { -1 } else { margin_min_us as i64 }
                                );
                            }
                            if let Phase::Running { relay, delay, .. } = &mut phase {
                                let (packets_sent, packets_acked) =
                                    server.connection(client).map_or((0, 0), |c| (c.stats.packets_sent, c.stats.packets_acked));
                                let w = delay::Window { frames, avg_frame_us, wait_us, stalls, rtt_us, packets_sent, packets_acked };
                                if let Some((d, why)) = delay.report(id, w, relay.active()) {
                                    let _ = server.broadcast(CH_RELIABLE, &ServerMsg::InputDelay { delay: d }.encode());
                                    server.flush();
                                    delay_changes += 1;
                                    logs.event(&format!("input delay -> {d} at frame {} ({why})", relay.next_frame()));
                                }
                            }
                        }
                        ClientMsg::Log { text } => logs.event(&format!("client {id}: {text}")),
                        ClientMsg::Ping { t } => {
                            let _ = server.send(client, CH_UNRELIABLE, &ServerMsg::Pong { t }.encode());
                        }
                        ClientMsg::NextRun { run: next } => {
                            // The client left the run (all players dead, it started a new
                            // game): the run's ticks stop waiting for and carrying it, as for a
                            // departed player; the next run starts once every client is ready.
                            let Phase::Running { relay, run, .. } = &mut phase else { continue };
                            let Some(p) = players.get_mut(&id) else { continue };
                            if next != *run + 1 || !p.in_run {
                                logs.event(&format!("client {id}: NextRun({next}) ignored in run {run}"));
                                continue;
                            }
                            p.in_run = false;
                            p.tick_ack = 0;
                            p.sent_input_ack = 0;
                            relay.remove(id);
                            logs.event(&format!("client {id} left run {run} at frame {}, ready for run {next}", relay.next_frame()));
                        }
                    }
                }
            }
        }

        // Lobby -> running once enough players said hello and their round trips are measured.
        if matches!(phase, Phase::Lobby) {
            let ready: Vec<u8> = players.iter().filter(|(_, p)| p.hello).map(|(i, _)| *i).collect();
            if ready.len() < args.players as usize {
                rtt_settle = None;
            } else {
                // The auto input delay needs each link's round trip. Samples taken while a
                // client was still loading the game carry its scheduling delays, so the
                // server probes every ready client a few times (Pong{t: 0}, acked like any
                // message) and uses each link's fastest sample.
                let now = Instant::now();
                let (since, base) = rtt_settle.get_or_insert_with(|| {
                    let base = ready.iter().map(|i| (*i, server.connection(*i as u16).map_or(0, |c| c.rtt_samples()))).collect();
                    (now, base)
                });
                let measured = args.input_delay.is_some()
                    || ready.iter().all(|i| {
                        server.connection(*i as u16).is_none_or(|c| c.rtt_samples() >= base.get(i).copied().unwrap_or(0) + RTT_PROBES)
                    });
                if !measured && now.duration_since(*since) < RTT_SETTLE_MAX {
                    if now.duration_since(last_rtt_probe) >= RTT_PROBE_EVERY {
                        for i in &ready {
                            let _ = server.send(*i as u16, CH_UNRELIABLE, &ServerMsg::Pong { t: 0 }.encode());
                        }
                        server.flush();
                        last_rtt_probe = now;
                    }
                } else {
                    rtt_settle = None;
                    let session_id = rudp_random();
                    let max_rtt = max_rtt_us(&server, &ready);
                    let input_delay = args.input_delay.unwrap_or_else(|| delay::initial(max_rtt));
                    let delay = delay::Controller::new(input_delay, args.input_delay.is_none(), max_rtt);
                    runs.clear();
                    phase = start_run(&mut server, &mut logs, &args, &mut players, ready, 0, session_id, delay, max_rtt);
                    had_session = true;
                }
            }
        }

        // Next run once every client left the current one (all ready).
        let all_ready = matches!(phase, Phase::Running { .. })
            && players.values().any(|p| p.hello)
            && players.values().filter(|p| p.hello).all(|p| !p.in_run);
        if all_ready {
            if let Phase::Running { relay, delay, started, session_id, seed, run } = std::mem::replace(&mut phase, Phase::Lobby) {
                for o in verifier.end_run(run) {
                    if o.chunk_mismatches > 0 || o.position_mismatches > 0 {
                        logs.event(&format!("MISMATCH {o:?}"));
                    }
                }
                runs.push(RunLog { run, seed, frames: relay.next_frame(), seconds: started.elapsed().as_secs_f64() });
                let ids: Vec<u8> = players.iter().filter(|(_, p)| p.hello).map(|(i, _)| *i).collect();
                let max_rtt = max_rtt_us(&server, &ids);
                // The input delay carries over (same network); its adaptation starts afresh.
                let delay = delay::Controller::new(delay.delay(), args.input_delay.is_none(), max_rtt);
                phase = start_run(&mut server, &mut logs, &args, &mut players, ids, run + 1, session_id, delay, max_rtt);
            }
        }

        if let Phase::Running { relay, run, .. } = &mut phase {
            let emitted = relay.advance();
            ticks_sent += emitted as u64;
            let now = Instant::now();
            let mut sent_any = false;
            for (id, p) in players.iter_mut() {
                if !p.in_run || !relay.active().contains(id) {
                    continue;
                }
                let input_ack = relay.input_ack(*id);
                let lacking = p.tick_ack < relay.next_frame();
                let due = emitted > 0
                    || input_ack != p.sent_input_ack
                    || (lacking && now.duration_since(p.last_stream_send) >= STREAM_RESEND);
                if !due {
                    continue;
                }
                let (first, ticks) = relay.ticks_from(p.tick_ack, protocol::STREAM_BUDGET);
                let msg = ServerMsg::Ticks { run: *run, input_ack, first, ticks };
                let _ = server.send(*id as u16, CH_UNRELIABLE, &msg.encode());
                p.sent_input_ack = input_ack;
                p.last_stream_send = now;
                sent_any = true;
            }
            if sent_any {
                server.flush();
            }
            let min_ack = relay.active().iter().filter_map(|id| players.get(id)).map(|p| p.tick_ack).min();
            if let Some(m) = min_ack {
                relay.trim(m);
            }
        }

        if last_metrics.elapsed() >= Duration::from_secs(1) {
            let dt = last_metrics.elapsed().as_secs_f64();
            last_metrics = Instant::now();
            let cpu_now = metrics::process_cpu_time();
            let cpu_frac = (cpu_now - cpu_last).as_secs_f64() / dt;
            cpu_last = cpu_now;
            cpu_peak = cpu_peak.max(cpu_frac);
            let ids: Vec<u8> = players.keys().copied().collect();
            for id in ids {
                let Some(conn) = server.connection(id as u16) else { continue };
                let (sent, recv, rtt) = (conn.stats.bytes_sent, conn.stats.bytes_received, conn.rtt_us());
                let p = players.get_mut(&id).unwrap();
                let out_rate = p.bytes_out.sample(sent, dt);
                let in_rate = p.bytes_in.sample(recv, dt);
                if let Some(f) = &mut logs.net {
                    let _ = writeln!(
                        f,
                        "{{\"t\":{:.3},\"player\":{id},\"out_Bps\":{out_rate:.0},\"in_Bps\":{in_rate:.0},\"rtt_us\":{rtt},\"cpu_cores\":{cpu_frac:.4}}}",
                        elapsed_secs()
                    );
                }
            }
            write_summary(&logs, &args, &phase, &runs, &players, &departed, &perf, &verifier, cpu_start, wall_start, cpu_peak, ticks_sent, delay_changes);
        }

        if args.exit_when_empty && had_session && players.is_empty() {
            logs.event("all players left; exiting");
            write_summary(&logs, &args, &phase, &runs, &players, &departed, &perf, &verifier, cpu_start, wall_start, cpu_peak, ticks_sent, delay_changes);
            return Ok(());
        }
        if players.is_empty() {
            expected_mods = None;
            if matches!(phase, Phase::Running { .. }) {
                logs.event("session ended (no players); back to lobby");
                phase = Phase::Lobby;
            }
        }
        wait_readable(server.socket(), Duration::from_millis(1));
    }
}

#[allow(clippy::too_many_arguments)]
fn write_summary(
    logs: &Logs,
    args: &Args,
    phase: &Phase,
    runs: &[RunLog],
    players: &BTreeMap<u8, Player>,
    departed: &[(u8, Player)],
    perf: &BTreeMap<u8, PerfAgg>,
    v: &verify::Verifier,
    cpu_start: Duration,
    wall_start: Instant,
    cpu_peak: f64,
    ticks: u64,
    delay_changes: u32,
) {
    let Some(dir) = &logs.dir else { return };
    let wall = wall_start.elapsed().as_secs_f64();
    let cpu = (metrics::process_cpu_time() - cpu_start).as_secs_f64();
    let t = v.totals;
    let mut s = String::new();
    s.push_str("{\n");
    // Runs: the finished ones, then the current one.
    let mut all_runs = runs.to_vec();
    if let Phase::Running { relay, delay, started, session_id, seed, run } = phase {
        s.push_str(&format!(
            "  \"session\": {{\"id\": \"{session_id:016x}\", \"run\": {run}, \"seed\": {seed}, \"frame\": {}, \"seconds\": {:.1}, \"input_delay\": {}, \"input_delay_changes\": {delay_changes}}},\n",
            relay.next_frame(),
            started.elapsed().as_secs_f64(),
            delay.delay()
        ));
        all_runs.push(RunLog { run: *run, seed: *seed, frames: relay.next_frame(), seconds: started.elapsed().as_secs_f64() });
    }
    s.push_str("  \"runs\": [");
    for (n, r) in all_runs.iter().enumerate() {
        let t = v.run_totals.get(&r.run).copied().unwrap_or_default();
        s.push_str(&format!(
            "{}\n    {{\"run\": {}, \"seed\": {}, \"frames\": {}, \"seconds\": {:.1}, \"checkpoints\": {}, \"chunk_comparisons\": {}, \"chunk_mismatches\": {}, \"position_comparisons\": {}, \"position_mismatches\": {}, \"entity_hash_mismatches\": {}}}",
            if n > 0 { "," } else { "" },
            r.run, r.seed, r.frames, r.seconds, t.checkpoints, t.chunk_comparisons, t.chunk_mismatches, t.position_comparisons,
            t.position_mismatches, t.entity_hash_mismatches
        ));
    }
    s.push_str("\n  ],\n");
    s.push_str(&format!("  \"players_required\": {},\n", args.players));
    s.push_str(&format!("  \"ticks_sent\": {ticks},\n"));
    s.push_str(&format!(
        "  \"verification\": {{\"checkpoints\": {}, \"chunk_comparisons\": {}, \"chunk_mismatches\": {}, \"position_comparisons\": {}, \"position_mismatches\": {}, \"entity_hash_mismatches\": {}, \"partial_chunk_sets\": {}, \"first_mismatch_run\": {}, \"first_mismatch_frame\": {}}},\n",
        t.checkpoints, t.chunk_comparisons, t.chunk_mismatches, t.position_comparisons, t.position_mismatches,
        t.entity_hash_mismatches, t.partial_chunk_sets,
        v.first_mismatch().map_or("null".to_string(), |(r, _)| r.to_string()),
        v.first_mismatch().map_or("null".to_string(), |(_, f)| f.to_string())
    ));
    s.push_str(&format!(
        "  \"server_cpu\": {{\"avg_cores\": {:.4}, \"peak_cores_1s\": {:.4}, \"wall_seconds\": {:.1}}},\n",
        if wall > 0.0 { cpu / wall } else { 0.0 },
        cpu_peak,
        wall
    ));
    s.push_str("  \"clients\": [");
    let all = players.iter().map(|(i, p)| (*i, p, true)).chain(departed.iter().map(|(i, p)| (*i, p, false)));
    for (n, (id, p, connected)) in all.enumerate() {
        if n > 0 {
            s.push(',');
        }
        let pf = perf.get(&id);
        s.push_str(&format!(
            "\n    {{\"player\": {id}, \"name\": {:?}, \"connected\": {connected}, \"avg_out_Bps\": {:.0}, \"peak_out_Bps\": {:.0}, \"avg_in_Bps\": {:.0}, \"peak_in_Bps\": {:.0}, \"perf_frames\": {}, \"avg_frame_us\": {:.0}, \"worst_p99_frame_us\": {}, \"tick_stalls\": {}}}",
            p.name,
            p.bytes_out.average(),
            p.bytes_out.max_per_sec,
            p.bytes_in.average(),
            p.bytes_in.max_per_sec,
            pf.map_or(0, |a| a.frames),
            pf.map_or(0.0, |a| if a.frames > 0 { a.sum_avg_us / a.frames as f64 } else { 0.0 }),
            pf.map_or(0, |a| a.worst_p99_us),
            pf.map_or(0, |a| a.stalls),
        ));
    }
    s.push_str("\n  ]\n}\n");
    let tmp = dir.join("summary.json.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(tmp, dir.join("summary.json"));
    }
}

fn rudp_random() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    h.finish()
}

/// Lobby round-trip measurement: probes per ready client, their interval, and the longest
/// the start waits for them.
const RTT_PROBES: u32 = 8;
const RTT_PROBE_EVERY: Duration = Duration::from_millis(25);
const RTT_SETTLE_MAX: Duration = Duration::from_secs(2);

/// Wait until a datagram is queued on the (non-blocking) `socket`, at most `timeout`, so the
/// relay forwards inputs and ticks as soon as they arrive.
fn wait_readable(socket: &std::net::UdpSocket, timeout: Duration) {
    let mut b = [0u8; 1];
    if socket.set_nonblocking(false).is_ok() && socket.set_read_timeout(Some(timeout)).is_ok() {
        // Returns as soon as a datagram is queued (on Windows with WSAEMSGSIZE for larger ones).
        let _ = socket.peek_from(&mut b);
    } else {
        std::thread::sleep(timeout);
    }
    let _ = socket.set_nonblocking(true);
}
