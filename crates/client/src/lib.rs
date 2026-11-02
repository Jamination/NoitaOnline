//! Noita Online client runtime, loaded into Noita by the mod through LuaJIT FFI.
//!
//! All exported functions are `extern "C"`, called from the game thread only. Networking runs
//! on a background thread (`net`), so packet handling never waits for the game's frames.

pub mod net;
#[cfg(all(windows, target_arch = "x86"))]
pub mod patches;
pub mod perf;
#[cfg(all(windows, target_arch = "x86"))]
pub mod world;

use net::{ConnectParams, Shared, Status};
use protocol::{ClientMsg, EntityPos, CH_RELIABLE};
use std::ffi::{c_char, CStr};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Runtime {
    shared: Arc<Shared>,
    net: Option<std::thread::JoinHandle<()>>,
    timer: perf::FrameTimer,
}

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

fn with_rt<T>(default: T, f: impl FnOnce(&mut Runtime) -> T) -> T {
    let mut g = match RUNTIME.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match g.as_mut() {
        Some(rt) => f(rt),
        None => default,
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        ""
    } else {
        CStr::from_ptr(p).to_str().unwrap_or("")
    }
}

/// ABI version of this library; the Lua side refuses to run against a different one.
#[no_mangle]
pub extern "C" fn no_abi_version() -> u32 {
    1
}

/// Initialise the runtime (idempotent).
#[no_mangle]
pub extern "C" fn no_init() -> i32 {
    let mut g = RUNTIME.lock().unwrap_or_else(|p| p.into_inner());
    if g.is_none() {
        *g = Some(Runtime { shared: Shared::new(), net: None, timer: perf::FrameTimer::default() });
    }
    0
}

/// Start connecting to `addr` ("host:port"). `netsim` may be empty or e.g.
/// "loss=0.05,latency=100". Returns 0 on success, negative on error.
///
/// # Safety
/// String arguments must be valid NUL-terminated strings (or null).
#[no_mangle]
pub unsafe extern "C" fn no_connect(
    addr: *const c_char,
    name: *const c_char,
    game_build: *const c_char,
    mod_hash: u64,
    netsim: *const c_char,
) -> i32 {
    let netsim = match cstr(netsim) {
        "" => None,
        s => match rudp::LinkConditions::parse(s) {
            Ok(c) => Some(c),
            Err(_) => return -3,
        },
    };
    let params = ConnectParams {
        addr: cstr(addr).to_owned(),
        name: cstr(name).to_owned(),
        game_build: cstr(game_build).to_owned(),
        mod_hash,
        netsim,
    };
    with_rt(-1, |rt| {
        if rt.net.is_some() {
            return -2;
        }
        match net::spawn(rt.shared.clone(), params) {
            Ok(h) => {
                rt.net = Some(h);
                0
            }
            Err(_) => -4,
        }
    })
}

/// 0 idle, 1 connecting, 2 lobby, 3 started, 4 disconnected.
#[no_mangle]
pub extern "C" fn no_status() -> i32 {
    with_rt(Status::Idle as i32, |rt| rt.shared.state.lock().unwrap().status.unwrap_or(Status::Idle) as i32)
}

#[repr(C)]
pub struct StartInfoC {
    pub seed: u32,
    pub input_delay: u16,
    pub player: u8,
    pub num_players: u8,
    pub session_id: u64,
    pub player_ids: [u8; 32],
}

/// Fill `out` once the session has started. Returns 1 if started, 0 otherwise.
///
/// # Safety
/// `out` must point at a writable `StartInfoC`.
#[no_mangle]
pub unsafe extern "C" fn no_start_info(out: *mut StartInfoC) -> i32 {
    with_rt(0, |rt| {
        let s = rt.shared.state.lock().unwrap();
        let Some(st) = &s.start else { return 0 };
        let o = &mut *out;
        o.seed = st.seed;
        o.input_delay = st.input_delay;
        o.player = st.player;
        o.num_players = st.players.len().min(32) as u8;
        o.session_id = st.session_id;
        o.player_ids = [0; 32];
        for (i, (id, _)) in st.players.iter().take(32).enumerate() {
            o.player_ids[i] = *id;
        }
        1
    })
}

/// Queue the local player's input for simulation frame `frame`.
///
/// # Safety
/// `data` must point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn no_send_input(frame: u32, data: *const u8, len: u32) -> i32 {
    let bytes = if len == 0 { Vec::new() } else { std::slice::from_raw_parts(data, len as usize).to_vec() };
    with_rt(-1, |rt| {
        let mut s = rt.shared.state.lock().unwrap();
        let Some(stream) = &mut s.stream else { return -2 };
        stream.push_input(frame, bytes);
        s.input_dirty = true;
        drop(s);
        rt.shared.net_cond.notify_one();
        0
    })
}

/// Block until the tick for `frame` arrives (or `timeout_ms`), then write it to `out` as
/// `[count u8] count × ([player u8][len u16 LE][bytes])`.
/// Returns bytes written, -1 timeout, -2 disconnected, -3 buffer too small.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn no_wait_tick(frame: u32, timeout_ms: u32, out: *mut u8, cap: u32) -> i32 {
    let shared = match with_rt(None, |rt| Some(rt.shared.clone())) {
        Some(s) => s,
        None => return -2,
    };
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms as u64);
    let mut s = shared.state.lock().unwrap();
    loop {
        if let Some(inputs) = s.stream.as_mut().and_then(|st| st.take_tick(frame)) {
            drop(s);
            let waited = started.elapsed().as_micros() as u64;
            with_rt((), |rt| rt.timer.window_wait_us += waited);
            let mut buf = Vec::with_capacity(64);
            buf.push(inputs.len().min(255) as u8);
            for (p, d) in inputs.iter().take(255) {
                buf.push(*p);
                buf.extend_from_slice(&(d.len().min(u16::MAX as usize) as u16).to_le_bytes());
                buf.extend_from_slice(&d[..d.len().min(u16::MAX as usize)]);
            }
            if buf.len() > cap as usize {
                // Caller bug: inputs are capped far below any sane buffer; fail loudly.
                return -3;
            }
            std::ptr::copy_nonoverlapping(buf.as_ptr(), out, buf.len());
            return buf.len() as i32;
        }
        if s.status == Some(Status::Disconnected) {
            return -2;
        }
        let now = Instant::now();
        if now >= deadline {
            return -1;
        }
        s = shared.game_cond.wait_timeout(s, deadline - now).unwrap().0;
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EntityPosC {
    pub id: u32,
    pub kind: u32,
    pub x: f32,
    pub y: f32,
}

/// Hash all loaded chunks now and send a state report for `frame` together with the given
/// entity hash and tracked positions. Returns the time spent hashing in µs.
///
/// # Safety
/// `positions` must point at `n` readable `EntityPosC`; must run on Noita's game thread.
#[no_mangle]
pub unsafe extern "C" fn no_report_state(frame: u32, entity_hash: u64, positions: *const EntityPosC, n: u32) -> i32 {
    let t0 = Instant::now();
    #[cfg(all(windows, target_arch = "x86"))]
    let chunks = world::hash_loaded_chunks();
    #[cfg(not(all(windows, target_arch = "x86")))]
    let chunks: Vec<protocol::ChunkHash> = Vec::new();
    let elapsed = t0.elapsed().as_micros() as i32;
    let pos = if n == 0 { &[][..] } else { std::slice::from_raw_parts(positions, n as usize) };
    let positions: Vec<EntityPos> = pos
        .iter()
        .map(|p| EntityPos { id: p.id, kind: p.kind as u8, x_bits: p.x.to_bits(), y_bits: p.y.to_bits() })
        .collect();
    with_rt((), |rt| rt.shared.send(CH_RELIABLE, &ClientMsg::StateHash { frame, entities: entity_hash, chunks, positions }));
    elapsed
}

/// Local-only: hash loaded chunks and return (count << 48) ^ combined hash, for diagnostics.
#[no_mangle]
pub extern "C" fn no_world_digest() -> u64 {
    #[cfg(all(windows, target_arch = "x86"))]
    unsafe {
        let chunks = world::hash_loaded_chunks();
        let mut h = 0u64;
        for c in &chunks {
            h = world::mix(h, c.hash ^ c.index as u64);
        }
        ((chunks.len() as u64) << 48) ^ (h & 0xffff_ffff_ffff)
    }
    #[cfg(not(all(windows, target_arch = "x86")))]
    0
}

/// Mark the start of a game frame. Every 60 frames a performance report is sent (when
/// connected). Returns the previous frame's duration in µs (0 for the first).
#[no_mangle]
pub extern "C" fn no_frame_mark() -> u32 {
    with_rt(0, |rt| {
        let dt = rt.timer.mark().unwrap_or(0);
        if rt.timer.window_len() >= 60 {
            if let Some(w) = rt.timer.take_window() {
                let connected = matches!(
                    rt.shared.state.lock().unwrap().status,
                    Some(Status::Lobby) | Some(Status::Started)
                );
                if connected {
                    rt.shared.send(
                        protocol::CH_RELIABLE,
                        &ClientMsg::Perf {
                            frames: w.frames,
                            avg_frame_us: w.avg_us,
                            p99_frame_us: w.p99_us,
                            max_frame_us: w.max_us,
                            wait_us: w.wait_us,
                        },
                    );
                }
            }
        }
        dt
    })
}

/// Write every recorded frame interval (µs, one per line) to `path`. Returns lines written.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn no_perf_dump(path: *const c_char) -> i32 {
    let path = cstr(path).to_owned();
    with_rt(-1, |rt| {
        let Ok(mut f) = std::fs::File::create(&path) else { return -1 };
        let mut s = String::with_capacity(rt.timer.all.len() * 6);
        for v in &rt.timer.all {
            s.push_str(&v.to_string());
            s.push('\n');
        }
        if f.write_all(s.as_bytes()).is_err() {
            return -1;
        }
        rt.timer.all.len() as i32
    })
}

/// Pop the next human-readable network event into `out` (NUL-terminated). Returns its
/// length, or 0 if there is none.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn no_poll_event(out: *mut u8, cap: u32) -> i32 {
    with_rt(0, |rt| {
        let Some(e) = rt.shared.state.lock().unwrap().events.pop_front() else { return 0 };
        let b = e.as_bytes();
        let n = b.len().min(cap.saturating_sub(1) as usize);
        std::ptr::copy_nonoverlapping(b.as_ptr(), out, n);
        *out.add(n) = 0;
        n as i32
    })
}

/// Forward a log line to the server.
///
/// # Safety
/// `text` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn no_log(text: *const c_char) {
    let text = cstr(text).to_owned();
    with_rt((), |rt| rt.shared.send(CH_RELIABLE, &ClientMsg::Log { text }));
}

/// Install the determinism patches into noita.exe (idempotent). Returns the applied-set
/// bitmask (>0) or -1 on failure (details in `no_last_error`).
#[no_mangle]
pub extern "C" fn no_apply_patches() -> i32 {
    #[cfg(all(windows, target_arch = "x86"))]
    unsafe {
        match patches::apply_all() {
            Ok(m) => m as i32,
            Err(e) => {
                set_last_error(e.0);
                -1
            }
        }
    }
    #[cfg(not(all(windows, target_arch = "x86")))]
    {
        set_last_error("patches only exist inside 32-bit Windows noita.exe".into());
        -1
    }
}

/// Reseed every engine RNG from the session seed (call at world init and frame 1).
#[no_mangle]
pub extern "C" fn no_fix_seeds(seed: u32) {
    #[cfg(all(windows, target_arch = "x86"))]
    unsafe {
        patches::fix_seeds(seed)
    };
    #[cfg(not(all(windows, target_arch = "x86")))]
    let _ = seed;
}

static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn set_last_error(s: String) {
    *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = s;
}

/// Copy the last error message into `out` (NUL-terminated); returns its length.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn no_last_error(out: *mut u8, cap: u32) -> i32 {
    let e = LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner());
    let n = e.len().min(cap.saturating_sub(1) as usize);
    std::ptr::copy_nonoverlapping(e.as_ptr(), out, n);
    *out.add(n) = 0;
    n as i32
}

/// Copy the session parameters ("key=value" text from the server) into `out`
/// (NUL-terminated). Returns the length, or -1 before the session started.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn no_start_params(out: *mut u8, cap: u32) -> i32 {
    with_rt(-1, |rt| {
        let s = rt.shared.state.lock().unwrap();
        let Some(st) = &s.start else { return -1 };
        let b = st.params.as_bytes();
        let n = b.len().min(cap.saturating_sub(1) as usize);
        std::ptr::copy_nonoverlapping(b.as_ptr(), out, n);
        *out.add(n) = 0;
        n as i32
    })
}

/// Block until the session started (returns 3), the connection failed (4), or timeout (-1).
#[no_mangle]
pub extern "C" fn no_wait_started(timeout_ms: u32) -> i32 {
    let Some(shared) = with_rt(None, |rt| Some(rt.shared.clone())) else { return 4 };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    let mut s = shared.state.lock().unwrap();
    loop {
        match s.status {
            Some(Status::Started) => return 3,
            Some(Status::Disconnected) => return 4,
            _ => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return -1;
        }
        s = shared.game_cond.wait_timeout(s, deadline - now).unwrap().0;
    }
}

/// Smoothed round-trip time to the server in µs (from pings).
#[no_mangle]
pub extern "C" fn no_rtt_us() -> u64 {
    with_rt(0, |rt| rt.shared.rtt_us.load(std::sync::atomic::Ordering::Relaxed))
}

/// Disconnect and stop the network thread.
#[no_mangle]
pub extern "C" fn no_shutdown() {
    let handle = with_rt(None, |rt| {
        rt.shared.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        rt.shared.game_cond.notify_all();
        rt.shared.net_cond.notify_all();
        rt.net.take()
    });
    if let Some(h) = handle {
        let _ = h.join();
    }
}
