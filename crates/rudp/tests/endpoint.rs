//! Server/client handshake and messaging over real localhost UDP sockets.

use rudp::endpoint::{Client, ClientState, DisconnectReason, Server, ServerEvent};
use rudp::linksim::LinkConditions;
use rudp::packet::DenyReason;
use rudp::Config;
use std::time::{Duration, Instant};

fn pump(server: &mut Server, clients: &mut [Client], dur: Duration) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    let end = Instant::now() + dur;
    while Instant::now() < end {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
        while let Some(e) = server.poll_event() {
            events.push(e);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    events
}

fn pump_until(
    server: &mut Server,
    clients: &mut [Client],
    timeout: Duration,
    mut done: impl FnMut(&Server, &[Client], &[ServerEvent]) -> bool,
) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        events.extend(pump(server, clients, Duration::from_millis(2)));
        if done(server, clients, &events) {
            return events;
        }
    }
    panic!("condition not reached in {timeout:?}; events: {events:?}");
}

fn connected(c: &Client) -> bool {
    matches!(c.state(), ClientState::Connected { .. })
}

#[test]
fn four_clients_connect_exchange_and_fifth_is_denied() {
    let mut server = Server::bind("127.0.0.1:0", 4, Config::default(), None).unwrap();
    let addr = server.local_addr().unwrap();
    let mut clients: Vec<Client> = (0..4)
        .map(|i| Client::connect(addr, &format!("p{i}"), Config::default(), None).unwrap())
        .collect();
    let events = pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, cs, _| {
        cs.iter().all(connected)
    });
    let mut names: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Connected { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(names, ["p0", "p1", "p2", "p3"]);
    let mut ids: Vec<u16> = clients
        .iter()
        .map(|c| match c.state() {
            ClientState::Connected { client_id, max_clients } => {
                assert_eq!(*max_clients, 4);
                *client_id
            }
            _ => unreachable!(),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, [0, 1, 2, 3]);

    // Fifth client is refused.
    let mut extra = vec![Client::connect(addr, "p4", Config::default(), None).unwrap()];
    pump_until(&mut server, &mut extra, Duration::from_secs(5), |_, cs, _| {
        !matches!(cs[0].state(), ClientState::Connecting)
    });
    assert_eq!(extra[0].state(), &ClientState::Disconnected(DisconnectReason::Denied(DenyReason::ServerFull)));

    // Every client sends to the server; the server broadcasts a reply.
    for (i, c) in clients.iter_mut().enumerate() {
        c.send(0, format!("hello from {i}").as_bytes()).unwrap();
    }
    let events = pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, _, ev| {
        ev.iter().filter(|e| matches!(e, ServerEvent::Message { .. })).count() == 4
    });
    for e in events {
        if let ServerEvent::Message { client, channel, data } = e {
            assert_eq!(channel, 0);
            let text = String::from_utf8(data).unwrap();
            assert!(text.starts_with("hello from "), "{text}");
            let _ = client;
        }
    }
    let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
    server.broadcast(0, &big).unwrap();
    let mut got = vec![None; 4];
    let end = Instant::now() + Duration::from_secs(5);
    while got.iter().any(Option::is_none) && Instant::now() < end {
        pump(&mut server, &mut clients, Duration::from_millis(2));
        for (i, c) in clients.iter_mut().enumerate() {
            if let Some((_, d)) = c.receive() {
                got[i] = Some(d);
            }
        }
    }
    assert!(got.iter().all(|g| g.as_deref() == Some(&big[..])));
}

#[test]
fn disconnect_frees_slot_and_reports_reason() {
    let mut server = Server::bind("127.0.0.1:0", 1, Config::default(), None).unwrap();
    let addr = server.local_addr().unwrap();
    let mut clients = vec![Client::connect(addr, "a", Config::default(), None).unwrap()];
    pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, cs, _| connected(&cs[0]));
    clients[0].disconnect();
    let events = pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, _, ev| {
        ev.iter().any(|e| matches!(e, ServerEvent::Disconnected { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::Disconnected { client: 0, reason: DisconnectReason::Requested }
    )));
    // Slot is reusable.
    let mut clients = vec![Client::connect(addr, "b", Config::default(), None).unwrap()];
    pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, cs, _| connected(&cs[0]));
}

#[test]
fn server_detects_silent_client_timeout() {
    let cfg = Config { timeout_us: 500_000, ..Config::default() };
    let mut server = Server::bind("127.0.0.1:0", 2, cfg.clone(), None).unwrap();
    let addr = server.local_addr().unwrap();
    let mut clients = vec![Client::connect(addr, "a", cfg, None).unwrap()];
    pump_until(&mut server, &mut clients, Duration::from_secs(5), |_, cs, _| connected(&cs[0]));
    // Client stops updating entirely (e.g. process frozen).
    let mut none: Vec<Client> = Vec::new();
    let events = pump_until(&mut server, &mut none, Duration::from_secs(3), |_, _, ev| {
        ev.iter().any(|e| matches!(e, ServerEvent::Disconnected { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::Disconnected { reason: DisconnectReason::TimedOut, .. }
    )));
}

#[test]
fn handshake_and_ordered_stream_survive_loss_and_latency_on_both_ends() {
    let cond = LinkConditions { loss: 0.05, latency_us: 100_000, jitter_us: 10_000, duplicate: 0.0 };
    let mut server = Server::bind("127.0.0.1:0", 4, Config::default(), Some(cond)).unwrap();
    let addr = server.local_addr().unwrap();
    let mut clients: Vec<Client> = (0..4)
        .map(|i| Client::connect(addr, &format!("p{i}"), Config::default(), Some(cond)).unwrap())
        .collect();
    pump_until(&mut server, &mut clients, Duration::from_secs(10), |_, cs, _| {
        cs.iter().all(connected)
    });
    let n = 300u32;
    for i in 0..n {
        server.broadcast(0, &i.to_le_bytes()).unwrap();
    }
    let mut next = [0u32; 4];
    let end = Instant::now() + Duration::from_secs(15);
    while next.iter().any(|&v| v < n) && Instant::now() < end {
        pump(&mut server, &mut clients, Duration::from_millis(2));
        for (i, c) in clients.iter_mut().enumerate() {
            while let Some((_, d)) = c.receive() {
                assert_eq!(u32::from_le_bytes(d[..].try_into().unwrap()), next[i]);
                next[i] += 1;
            }
        }
    }
    assert_eq!(next, [n; 4]);
    // Clients only send acks/keepalives here, so their RTT samples arrive slowly: wait for
    // them. Both ends add 100 ms each way, so the RTT must be ~400 ms (one end only: ~200).
    let rtts = |cs: &[Client]| cs.iter().map(|c| c.connection().unwrap().rtt_us()).collect::<Vec<_>>();
    let end = Instant::now() + Duration::from_secs(10);
    while rtts(&clients).iter().any(|&r| r < 380_000) && Instant::now() < end {
        pump(&mut server, &mut clients, Duration::from_millis(5));
    }
    for rtt in rtts(&clients) {
        assert!(rtt >= 380_000, "both ends add 100ms each way: rtt {rtt}");
    }
}
