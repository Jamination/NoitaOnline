//! Headless simulated clients for exercising the server without Noita: each bot joins,
//! runs the lockstep loop at 60 Hz (input for frame f+delay, wait for tick f), and reports
//! a state hash derived deterministically from the ticks it consumed, so any relay bug
//! (missing, reordered or altered input) shows up as a verification mismatch.

use protocol::stream::ClientStream;
use protocol::{ChunkHash, ClientMsg, EntityPos, ServerMsg, CH_RELIABLE, CH_UNRELIABLE};
use rudp::{Client, ClientState, LinkConditions};
use std::time::{Duration, Instant};

fn mix(h: u64, v: u64) -> u64 {
    let mut x = h ^ v.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^ (x >> 29)
}

struct Bot {
    client: Client,
    hello_sent: bool,
    id: u8,
    delay: u32,
    stream: Option<ClientStream>,
    frame: u32,
    state: u64,
    next_input: u32,
    next_frame_at: Instant,
    last_stream_send: Instant,
    stalled_frames: u64,
    /// The run being played (`Start.run`); None after leaving it, until the next Start.
    run: Option<u32>,
}

const STREAM_RESEND: Duration = Duration::from_millis(16);

fn main() {
    let mut addr = "127.0.0.1:41773".to_string();
    let mut bots = 4usize;
    let mut frames = 3600u32;
    let mut netsim: Option<LinkConditions> = None;
    let mut input_bytes = 6usize;
    let mut runs = 1u32;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_default();
        match k.as_str() {
            "--server" => addr = v,
            "--bots" => bots = v.parse().expect("--bots"),
            "--frames" => frames = v.parse().expect("--frames"),
            "--netsim" => netsim = Some(LinkConditions::parse(&v).expect("--netsim")),
            "--input-bytes" => input_bytes = v.parse().expect("--input-bytes"),
            // Every run but the last ends like an all-dead game over: bot i leaves it (NextRun)
            // at frame `frames + 60 * i`, the others keep playing it until they leave.
            "--runs" => runs = v.parse().expect("--runs"),
            _ => {
                eprintln!("simclient [--server ADDR] [--bots N] [--frames N] [--netsim SPEC] [--input-bytes N] [--runs N]");
                std::process::exit(2);
            }
        }
    }
    let mut all: Vec<Bot> = (0..bots)
        .map(|i| Bot {
            client: Client::connect(addr.as_str(), &format!("bot{i}"), protocol::transport_config(), netsim).expect("connect"),
            hello_sent: false,
            id: 0,
            delay: 0,
            stream: None,
            frame: 0,
            state: 0,
            next_input: 0,
            next_frame_at: Instant::now(),
            last_stream_send: Instant::now(),
            stalled_frames: 0,
            run: None,
        })
        .collect();
    let t0 = Instant::now();
    loop {
        let mut done = 0;
        for (i, b) in all.iter_mut().enumerate() {
            b.client.update();
            match b.client.state() {
                ClientState::Connected { .. } if !b.hello_sent => {
                    let h = ClientMsg::Hello {
                        protocol: protocol::GAME_PROTOCOL_VERSION,
                        game_build: "simclient".into(),
                        mod_hash: 0,
                        name: format!("bot{i}"),
                    };
                    b.client.send(CH_RELIABLE, &h.encode()).unwrap();
                    b.hello_sent = true;
                }
                ClientState::Disconnected(r) => {
                    eprintln!("bot{i} disconnected: {r:?}");
                    std::process::exit(1);
                }
                _ => {}
            }
            while let Some((_, d)) = b.client.receive() {
                match ServerMsg::decode(&d) {
                    Ok(ServerMsg::Welcome { player, .. }) => b.id = player,
                    Ok(ServerMsg::Start { run, input_delay, seed, .. }) => {
                        b.run = Some(run);
                        b.frame = 0;
                        b.delay = input_delay as u32;
                        b.state = seed as u64;
                        b.next_input = b.delay;
                        b.stream = Some(ClientStream::new(run, b.delay));
                        b.next_frame_at = Instant::now();
                    }
                    Ok(ServerMsg::Ticks { run, input_ack, first, ticks }) => {
                        if let Some(s) = &mut b.stream {
                            s.on_ticks(run, input_ack, first, ticks);
                        }
                    }
                    // A shorter delay pauses submitting until the simulation catches up; a
                    // longer one submits further ahead (the inputs stay contiguous).
                    Ok(ServerMsg::InputDelay { delay }) => b.delay = delay as u32,
                    Ok(ServerMsg::Kick { reason }) => {
                        eprintln!("bot{i} kicked: {reason}");
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
            let Some(run) = b.run else { continue };
            let Some(stream) = &mut b.stream else { continue };
            let last_run = run + 1 >= runs;
            let leave_at = frames + 60 * i as u32;
            if !last_run && b.frame >= leave_at {
                let _ = b.client.send(CH_RELIABLE, &ClientMsg::NextRun { run: run + 1 }.encode());
                b.client.flush();
                b.run = None;
                b.stream = None;
                continue;
            }
            let end = if last_run { frames } else { leave_at };
            let mut pushed = false;
            // Keep input submitted `delay` frames ahead of the simulation.
            while b.next_input <= b.frame + b.delay && b.next_input < end + b.delay {
                let f = b.next_input;
                let data: Vec<u8> =
                    (0..input_bytes).map(|k| (mix(f as u64 ^ ((b.id as u64) << 40), k as u64) & 0xff) as u8).collect();
                stream.push_input(f, data);
                b.next_input += 1;
                pushed = true;
            }
            let now = Instant::now();
            if pushed || (stream.has_unacked_inputs() && now.duration_since(b.last_stream_send) >= STREAM_RESEND) {
                let _ = b.client.send(CH_UNRELIABLE, &stream.inputs_msg().encode());
                b.client.flush();
                b.last_stream_send = now;
            }
            // Advance at most at 60 Hz, only when the tick is present.
            if b.frame < end && now >= b.next_frame_at {
                if let Some(inputs) = stream.take_tick(b.frame) {
                    for (p, d) in &inputs {
                        b.state = mix(b.state, *p as u64);
                        for &x in d {
                            b.state = mix(b.state, x as u64);
                        }
                    }
                    if b.frame % 60 == 59 {
                        let rep = ClientMsg::StateHash {
                            frame: b.frame,
                            entities: Some(b.state),
                            chunks: vec![ChunkHash { index: 0, hash: b.state }],
                            positions: vec![EntityPos { id: 1, kind: 0, x_bits: (b.state >> 32) as u32, y_bits: b.state as u32 }],
                        };
                        b.client.send(CH_RELIABLE, &rep.encode()).unwrap();
                    }
                    b.frame += 1;
                    // Catch up if we fell behind, but never burst more than a few frames.
                    b.next_frame_at = (b.next_frame_at + Duration::from_micros(16_667)).max(now - Duration::from_millis(50));
                } else {
                    b.stalled_frames += 1;
                }
            }
            if last_run && b.frame >= frames {
                done += 1;
            }
        }
        if done == bots {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Let the final reports flush.
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        for b in &mut all {
            b.client.update();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    for b in &mut all {
        b.client.disconnect();
    }
    let secs = t0.elapsed().as_secs_f64();
    println!("simclient: {bots} bots x {frames} frames in {secs:.1}s (ideal {:.1}s)", frames as f64 / 60.0);
    for (i, b) in all.iter().enumerate() {
        println!("bot{i}: state={:016x} delay={} stalled_loop_iterations={}", b.state, b.delay, b.stalled_frames);
    }
}
