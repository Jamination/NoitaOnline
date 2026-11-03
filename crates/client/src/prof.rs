//! Diagnostics: frame-section profiler (`NOITA_ONLINE_PROF=1`). Wraps top-level engine calls
//! of the frame (DeathMatch::Update and its children, GridWorld::Update's steps, the mod Lua
//! dispatches, the world draw) and writes their inclusive main-thread time per frame every
//! 600 frames to `$NOITA_ONLINE_OUT/sections_<instance>.txt`. Works with and without the
//! determinism patches (install after them: wrappers call whatever the site calls now).
//!
//! `NOITA_ONLINE_STEPLOG=from-to` uses the same wrappers to find the stage of a step where
//! clients first differ: in that frame range it logs [`crate::patches::step_state`] at every
//! section edge (`e<i>` entry, `x<i>` exit of `SECTIONS[i]`), after every entity system's
//! update (`s<i>`, systems in update order, named by `N s<i> <RTTI name>` header lines) and at
//! the simulation window's entry and exit (`we`, `wx`), as `<frame> <point> <state>` lines in
//! `$NOITA_ONLINE_OUT/step_<instance>.txt`.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

type Bool = i32;
extern "system" {
    fn VirtualProtect(addr: *mut u8, size: usize, new: u32, old: *mut u32) -> Bool;
    fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
    fn FlushInstructionCache(process: isize, addr: *const u8, size: usize) -> Bool;
    fn GetCurrentProcess() -> isize;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: Bool, id: u32) -> isize;
    fn SuspendThread(thread: isize) -> u32;
    fn ResumeThread(thread: isize) -> u32;
    fn GetThreadContext(thread: isize, ctx: *mut u8) -> Bool;
    fn GetModuleHandleExW(flags: u32, addr: *const u16, module: *mut isize) -> Bool;
    fn GetModuleFileNameW(module: isize, name: *mut u16, size: u32) -> u32;
    fn Sleep(ms: u32);
}

/// Lockstep tick wait (µs), added by `no_wait_tick`.
pub static WAIT_US: AtomicU64 = AtomicU64::new(0);

enum Hook {
    /// `call rel32` at this address (main thread, not re-entered).
    Call(u32),
    /// Vtable slot holding a method pointer.
    Vtbl(u32),
}

/// Wrapped sections (build 0x6794ee3c). Indented names are nested in the one above.
const SECTIONS: &[(&str, Hook)] = &[
    ("dm_update (DeathMatch::Update)", Hook::Vtbl(0x0100_cdec)),
    ("  lua_pre (OnWorldPreUpdate dispatch)", Hook::Call(0x006b_3220)),
    ("  db6a10", Hook::Call(0x006b_3279)),
    ("  camera_update", Hook::Call(0x006b_3284)),
    ("  world_update (6f1d40)", Hook::Call(0x006b_32ae)),
    ("    gridworld_update (71a6b0)", Hook::Call(0x006f_1eed)),
    ("      box2d_join", Hook::Call(0x0071_a708)),
    ("      6f9210", Hook::Call(0x0071_a741)),
    ("      bodies_to_grid (718d50)", Hook::Call(0x0071_a748)),
    ("      719670", Hook::Call(0x0071_a74f)),
    ("      811260", Hook::Call(0x0071_a75a)),
    ("      71a590", Hook::Call(0x0071_a769)),
    ("      grid_sim (726340)", Hook::Call(0x0071_a779)),
    ("      71fde0", Hook::Call(0x0071_a780)),
    ("      71d020", Hook::Call(0x0071_a787)),
    ("      71cd10", Hook::Call(0x0071_a7bf)),
    ("      box2d_step_submit", Hook::Call(0x0071_a7f2)),
    ("      chunk_integrate", Hook::Call(0x0071_a825)),
    ("    lightfog_submit", Hook::Call(0x006f_1f13)),
    ("  marker_after_entities", Hook::Call(0x006b_32ca)),
    ("  42d610", Hook::Call(0x006b_32d8)),
    ("  57a4a0", Hook::Call(0x006b_32e4)),
    ("  streaming (743990)", Hook::Call(0x006b_32e9)),
    ("  lua_post (OnWorldPostUpdate dispatch)", Hook::Call(0x006b_330f)),
    ("  particle_render_prep (7148f0)", Hook::Call(0x006b_3a97)),
    ("  render_tiles (722ef0)", Hook::Call(0x006b_3aa4)),
    ("dm_draw (vtbl+0x10)", Hook::Vtbl(0x0100_cdf0)),
    ("  world_draw (6f21d0)", Hook::Call(0x006b_3b49)),
    ("    draw_async_submit (8909b0)", Hook::Call(0x006f_2272)),
    ("    890620", Hook::Call(0x006f_227d)),
    ("    c43f10", Hook::Call(0x006f_234c)),
    ("    b18750", Hook::Call(0x006f_2385)),
    ("    8466b0", Hook::Call(0x006f_2501)),
    ("    90c1b0", Hook::Call(0x006f_25c6)),
    ("    87d9a0", Hook::Call(0x006f_2873)),
    ("    88ca30", Hook::Call(0x006f_2b63)),
    ("  6c1ec0", Hook::Call(0x006b_3dca)),
    ("  8279c0", Hook::Call(0x006b_3e42)),
    ("  6233a0", Hook::Call(0x006b_3e9f)),
    ("  a8ccc0", Hook::Call(0x006b_3ed4)),
    ("  6ac450", Hook::Call(0x006b_3edd)),
    ("  daf0d0", Hook::Call(0x006b_3f27)),
    ("  875310", Hook::Call(0x006b_3fc7)),
    ("swap_window (SDL_GL_SwapWindow wrapper)", Hook::Vtbl(0x0105_3238)),
];
const DM_UPDATE: usize = 0;
const WORLD: usize = 4;
const AFTER_ENTITIES: usize = 19;
/// The entity system manager's update (`FUN_0057a4a0`: every system's vtable+0xc).
const SYSTEMS: usize = 21;
const N: usize = SECTIONS.len();
/// Entity system manager (singleton of `FUN_0042d610`); +0x14/+0x18 system pointer vector.
const SYSTEM_MANAGER: u32 = 0x0122_36e0;

struct State {
    start: [Option<Instant>; N],
    last_exit: [Option<Instant>; N],
    total: [Duration; N],
    calls: [u64; N],
    entities: Duration,
    frames: u32,
    window_start: Option<Instant>,
    /// Sums over the window, sampled every 10th frame at `render_tiles` exit: see [`COUNTERS`].
    counts: [f64; NC],
    samples: u32,
}

static STATE: Mutex<State> = Mutex::new(State {
    start: [None; N],
    last_exit: [None; N],
    total: [Duration::ZERO; N],
    calls: [0; N],
    entities: Duration::ZERO,
    frames: 0,
    window_start: None,
    counts: [0.0; NC],
    samples: 0,
});

const RENDER_TILES: usize = 25;
const COUNTERS: [&str; 18] = [
    "drawgrid rects", "cam w", "cam h", "particles a", "particles b", "particles c", "active tiles", "deferred tiles", "bodies", "chunks",
    "entities", "enemies", "projectiles", "items", "player x", "player y", "cam x", "cam y",
];
const NC: usize = COUNTERS.len();

/// Workload counters for [`State::counts`] (game thread, after the render tiles dispatch).
unsafe fn counters() -> [f64; NC] {
    use crate::patches::{rd, GAME_GLOBAL_PTR};
    let gg = rd(GAME_GLOBAL_PTR);
    let wd = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
    if gw == 0 {
        return [0.0; NC];
    }
    let gp = rd(gw + 0x540);
    let len = |o: u32| if gp != 0 { (rd(gp + o + 4).wrapping_sub(rd(gp + o)) / 0x70) as f64 } else { 0.0 };
    // Camera int rect: +0x498 x, +0x49c y, +0x4a0 width, +0x4a4 height (cells).
    let (w, h) = (rd(gw + 0x4a0) as i32, rd(gw + 0x4a4) as i32);
    let imp = rd(gw + 0x5d0);
    let (tiles, deferred) = if imp != 0 { (rd(imp) as f64, rd(imp + 0x30) as f64) } else { (0.0, 0.0) };
    let b2 = rd(gw + 0x530);
    let mut bodies = 0u64;
    let mut body = if b2 != 0 { rd(b2 + 0x1922c) } else { 0 };
    while body != 0 && bodies < 100_000 {
        bodies += 1;
        body = rd(body + 0xb8);
    }
    let mut chunks = 0u64;
    if let Some(wp) = crate::world::world_ptrs() {
        crate::world::for_each_chunk(&wp, |_, _| chunks += 1);
    }
    let (ents, (px, py)) = crate::digest::census();
    // Camera rect (GG+0xc: float x, y, w, h): its centre.
    let cam = |i: u32| f32::from_bits(rd(wd + 4 * i)) as f64;
    [
        rd(0x0120_7008) as f64,
        w.max(0) as f64,
        h.max(0) as f64,
        len(0x34),
        len(0x40),
        len(0x4c),
        tiles,
        deferred,
        bodies as f64,
        chunks as f64,
        ents[0] as f64,
        ents[1] as f64,
        ents[2] as f64,
        ents[3] as f64,
        px as f64,
        py as f64,
        cam(0) + cam(2) * 0.5,
        cam(1) + cam(3) * 0.5,
    ]
}

extern "C" fn enter(i: u32) {
    let now = Instant::now();
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let i = i as usize;
    s.start[i] = Some(now);
    if i == AFTER_ENTITIES {
        if let Some(t) = s.last_exit[WORLD] {
            s.entities += now.duration_since(t);
        }
    }
    if i == DM_UPDATE && s.window_start.is_none() {
        s.window_start = Some(now);
    }
    drop(s);
    if STEPLOG.is_some() {
        if i == SYSTEMS {
            unsafe { hook_systems() };
        }
        step_point('e', i as u32);
    }
}

extern "C" fn exit(i: u32) {
    if STEPLOG.is_some() {
        step_point('x', i);
    }
    let now = Instant::now();
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let i = i as usize;
    if let Some(t) = s.start[i].take() {
        s.total[i] += now.duration_since(t);
        s.calls[i] += 1;
    }
    s.last_exit[i] = Some(now);
    if i == RENDER_TILES && *PROF_ON && (s.frames % 10 == 0 || *PROF_WINDOW < 10) {
        let c = unsafe { counters() };
        for (a, b) in s.counts.iter_mut().zip(c) {
            *a += b;
        }
        s.samples += 1;
    }
    if i != DM_UPDATE {
        return;
    }
    if let Some((_, _, log)) = STEPLOG.as_ref() {
        let _ = log.lock().unwrap_or_else(|p| p.into_inner()).flush();
    }
    if !*PROF_ON {
        return;
    }
    s.frames += 1;
    if s.frames < *PROF_WINDOW {
        return;
    }
    let wall = s.window_start.map_or(Duration::ZERO, |t| now.duration_since(t));
    let n = s.frames as f64;
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / n;
    let wait = Duration::from_micros(WAIT_US.swap(0, Ordering::Relaxed));
    let mut out = format!("{} frames: wall {:.2} ms/frame, tick wait {:.2}, entities (world exit -> marker) {:.2}\n", s.frames, per(wall), per(wait), per(s.entities));
    let k = s.samples.max(1) as f64;
    let line: Vec<String> = COUNTERS.iter().zip(s.counts).map(|(name, c)| format!("{name} {:.1}", c / k)).collect();
    out.push_str(&format!("  counters: {}\n", line.join(", ")));
    let mut set = Vec::new();
    if let Some(wp) = unsafe { crate::world::world_ptrs() } {
        unsafe { crate::world::for_each_chunk(&wp, |idx, _| set.push(format!("{},{}", (idx % 512) as i32 - 256, (idx / 512) as i32 - 256))) };
    }
    out.push_str(&format!("  chunkset: {}\n", set.join(" ")));
    s.counts = [0.0; NC];
    s.samples = 0;
    for (k, (name, _)) in SECTIONS.iter().enumerate() {
        out.push_str(&format!("  {name:<44} {:>7.3} ms/frame  {:>6.2} calls/frame\n", per(s.total[k]), s.calls[k] as f64 / n));
    }
    s.total = [Duration::ZERO; N];
    s.calls = [0; N];
    s.entities = Duration::ZERO;
    s.frames = 0;
    s.window_start = Some(now);
    drop(s);
    if let (Ok(dir), inst) = (std::env::var("NOITA_ONLINE_OUT"), std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default()) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::path::Path::new(&dir).join(format!("sections_{inst}.txt"))) {
            let _ = f.write_all(out.as_bytes());
        }
        if let Some(samples) = SAMPLES.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::path::Path::new(&dir).join(format!("samples_{inst}.txt"))) {
                let mut text = format!("W {} {}\n", n as u32, samples.total);
                // Module of every 64 KiB region outside noita.exe: "M region base path".
                let mut regions: Vec<u32> = samples.inclusive.keys().map(|a| a >> 16).filter(|r| !(0x40..0x130).contains(r)).collect();
                regions.sort_unstable();
                regions.dedup();
                for r in regions {
                    let mut module = 0isize;
                    let mut name = [0u16; 260];
                    // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | _UNCHANGED_REFCOUNT
                    if unsafe { GetModuleHandleExW(0x4 | 0x2, (r << 16) as usize as *const u16, &mut module) } != 0 {
                        let len = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), 260) } as usize;
                        text.push_str(&format!("M {r:04x} {:08x} {}\n", module as u32, String::from_utf16_lossy(&name[..len])));
                    }
                }
                for (tag, map) in [("E", &samples.exclusive), ("I", &samples.inclusive)] {
                    for (a, c) in map.iter().filter(|(_, &c)| c >= 2) {
                        text.push_str(&format!("{tag} {a:08x} {c}\n"));
                    }
                }
                let _ = f.write_all(text.as_bytes());
            }
        }
    }
}

static PROF_ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("NOITA_ONLINE_PROF").is_some());
/// `NOITA_ONLINE_PROF_WINDOW=n`: frames per `sections_<id>.txt` window (default 600).
static PROF_WINDOW: LazyLock<u32> =
    LazyLock::new(|| std::env::var("NOITA_ONLINE_PROF_WINDOW").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(600));

/// `NOITA_ONLINE_STEPLOG=from-to`: (from, to, `step_<instance>.txt`).
#[allow(clippy::type_complexity)]
static STEPLOG: LazyLock<Option<(u32, u32, Mutex<std::io::BufWriter<std::fs::File>>)>> = LazyLock::new(|| {
    let v = std::env::var("NOITA_ONLINE_STEPLOG").ok()?;
    let (a, b) = v.split_once('-')?;
    let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let f = std::fs::File::create(std::path::Path::new(&out).join(format!("step_{inst}.txt"))).ok()?;
    Some((a.parse().ok()?, b.parse().ok()?, Mutex::new(std::io::BufWriter::new(f))))
});

/// `NOITA_ONLINE_STEPLOG_TILES=1`: also log [`crate::patches::step_tiles`] (`T <frame> <point>
/// ...` lines) at the window entry and at the grid update's entry and exit.
static STEPLOG_TILES: LazyLock<bool> = LazyLock::new(|| std::env::var_os("NOITA_ONLINE_STEPLOG_TILES").is_some());

/// `NOITA_ONLINE_STEPLOG_LITE=1`: instead of one line per point, one `L <frame> <point>:<hash>
/// ...` line per frame with [`crate::patches::step_hash`] at every point (cheap enough for a
/// whole run; the full state of a differing point then needs a STEPLOG rerun).
static STEPLOG_LITE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("NOITA_ONLINE_STEPLOG_LITE").is_some());
/// The LITE line being collected: (frame, text).
static LITE_LINE: Mutex<(u32, String)> = Mutex::new((0, String::new()));

/// Log the step state at `point` (`NOITA_ONLINE_STEPLOG`; no-op outside its frame range).
pub(crate) fn step_mark(point: &str) {
    let Some((a, b, log)) = STEPLOG.as_ref() else { return };
    if *STEPLOG_LITE {
        if let Some((frame, h)) = unsafe { crate::patches::step_hash(*a, *b) } {
            let mut line = LITE_LINE.lock().unwrap_or_else(|p| p.into_inner());
            if line.0 != frame && !line.1.is_empty() {
                let mut log = log.lock().unwrap_or_else(|p| p.into_inner());
                let _ = writeln!(log, "L {}{}", line.0, line.1);
                line.1.clear();
            }
            line.0 = frame;
            line.1.push_str(&format!(" {point}:{h:08x}"));
        }
        return;
    }
    if let Some((frame, state)) = unsafe { crate::patches::step_state(*a, *b) } {
        let mut log = log.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(log, "{frame} {point} {state}");
        if *STEPLOG_TILES && matches!(point, "we" | "e12" | "x12") {
            let _ = writeln!(log, "T {frame} {point} {}", unsafe { crate::patches::step_tiles() });
        }
    }
}

fn step_point(tag: char, i: u32) {
    step_mark(&format!("{tag}{i}"));
}

extern "C" fn system_exit(i: u32) {
    step_point('s', i);
}

/// RTTI name of the class whose vtable is `vtbl` (MSVC: vtable[-1] = complete object locator,
/// +0xc type descriptor, +8 decorated name).
unsafe fn rtti_name(vtbl: u32) -> String {
    let image = 0x0040_0000..0x0140_0000;
    let col = *((vtbl - 4) as *const u32);
    if !image.contains(&col) {
        return "?".into();
    }
    let td = *((col + 0xc) as *const u32);
    if !image.contains(&td) {
        return "?".into();
    }
    std::ffi::CStr::from_ptr((td + 8) as *const std::ffi::c_char).to_string_lossy().into_owned()
}

/// Wrap every entity system's update (vtable+0xc) so [`system_exit`] logs the state after it
/// (`NOITA_ONLINE_STEPLOG`). Runs once, on entry of the system manager's update.
unsafe fn hook_systems() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let (begin, end) = (*((SYSTEM_MANAGER + 0x14) as *const u32), *((SYSTEM_MANAGER + 0x18) as *const u32));
    let count = end.saturating_sub(begin) / 4;
    let page = VirtualAlloc(std::ptr::null_mut(), 0x2_0000, 0x3000, 0x40) as u32;
    if page == 0 || count == 0 {
        return;
    }
    let exit_cb = (system_exit as extern "C" fn(u32)) as u32;
    let mut at = page;
    let mut hooked = Vec::new();
    let mut names = String::new();
    for k in 0..count {
        let system = *((begin + 4 * k) as *const u32);
        if system == 0 {
            continue;
        }
        let vtbl = *(system as *const u32);
        let slot = vtbl + 0xc;
        names.push_str(&format!("N s{k} {} {:08x}\n", rtti_name(vtbl), *(slot as *const u32)));
        if hooked.contains(&slot) || at + 0x100 > page + 0x2_0000 {
            continue;
        }
        hooked.push(slot);
        let target = *(slot as *const u32);
        let stub = wrap(&mut at, k, target, None, exit_cb);
        write(slot, &stub.to_le_bytes());
    }
    if let Some((_, _, log)) = STEPLOG.as_ref() {
        let _ = log.lock().unwrap_or_else(|p| p.into_inner()).write_all(names.as_bytes());
    }
}

/// Sampled main-thread stacks of one report window: exact EIP counts and, per address on the
/// EBP return chain, the number of samples it appeared in.
#[derive(Default)]
struct Samples {
    total: u32,
    exclusive: std::collections::HashMap<u32, u32>,
    inclusive: std::collections::HashMap<u32, u32>,
}

static SAMPLES: Mutex<Option<Samples>> = Mutex::new(None);

/// Diagnostics (`NOITA_ONLINE_SAMPLE=1` with `NOITA_ONLINE_PROF=1`): sample the game thread
/// about every millisecond. Nothing is allocated while the thread is suspended (it may hold
/// the heap lock).
fn sampler(thread: isize) {
    const DEPTH: usize = 32;
    #[repr(C, align(16))]
    struct Ctx([u8; 0x2cc]);
    let mut ctx = Ctx([0; 0x2cc]);
    loop {
        let mut chain = [0u32; DEPTH];
        let mut n = 0;
        unsafe {
            Sleep(1);
            let p = ctx.0.as_mut_ptr();
            *(p as *mut u32) = 0x0001_0003; // CONTEXT_CONTROL | CONTEXT_INTEGER (Wine fills Ebp only with the latter)
            if SuspendThread(thread) == u32::MAX {
                continue;
            }
            if GetThreadContext(thread, p) != 0 {
                let r = |off: usize| *(p.add(off) as *const u32);
                let (mut ebp, eip, esp) = (r(0xb4), r(0xb8), r(0xc4));
                chain[0] = eip;
                n = 1;
                while n < DEPTH && ebp >= esp && ebp < esp.saturating_add(0x10_0000) && ebp & 3 == 0 {
                    let next = *(ebp as *const u32);
                    chain[n] = *((ebp + 4) as *const u32);
                    n += 1;
                    if next <= ebp {
                        break;
                    }
                    ebp = next;
                }
            }
            ResumeThread(thread);
        }
        if n == 0 {
            continue;
        }
        let mut g = SAMPLES.lock().unwrap_or_else(|p| p.into_inner());
        let s = g.get_or_insert_with(Samples::default);
        s.total += 1;
        *s.exclusive.entry(chain[0]).or_default() += 1;
        for k in 0..n {
            if !chain[..k].contains(&chain[k]) {
                *s.inclusive.entry(chain[k]).or_default() += 1;
            }
        }
    }
}

/// pushad; save xmm0-7 (engine code passes arguments in XMM registers); push idx; call cb;
/// restore; popad.
fn callback(code: &mut Vec<u8>, idx: u32, cb: u32) {
    code.extend_from_slice(&[0x60, 0x81, 0xEC, 0x80, 0x00, 0x00, 0x00]);
    for r in 0..8u8 {
        code.extend_from_slice(&[0xF3, 0x0F, 0x7F, 0x44 | (r << 3), 0x24, 16 * r]);
    }
    code.push(0x68);
    code.extend_from_slice(&idx.to_le_bytes());
    code.push(0xB8);
    code.extend_from_slice(&cb.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0, 0x83, 0xC4, 0x04]);
    for r in 0..8u8 {
        code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x44 | (r << 3), 0x24, 16 * r]);
    }
    code.extend_from_slice(&[0x81, 0xC4, 0x80, 0x00, 0x00, 0x00, 0x61]);
}

unsafe fn write(addr: u32, bytes: &[u8]) {
    let mut old = 0u32;
    VirtualProtect(addr as *mut u8, bytes.len(), 0x40, &mut old);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
    VirtualProtect(addr as *mut u8, bytes.len(), old, &mut old);
    FlushInstructionCache(GetCurrentProcess(), addr as *const u8, bytes.len());
}

/// Write a wrapper stub for `target` at `*at` (advanced past it) and return its address:
/// a return-address slot (sites are not re-entered), then
/// `[enter idx]; pop [slot]; call target; push [slot]; exit idx; ret`.
unsafe fn wrap(at: &mut u32, idx: u32, target: u32, enter_cb: Option<u32>, exit_cb: u32) -> u32 {
    let slot = *at;
    let stub = slot + 4;
    let mut c = Vec::with_capacity(256);
    if let Some(cb) = enter_cb {
        callback(&mut c, idx, cb);
    }
    c.extend_from_slice(&[0x8F, 0x05]);
    c.extend_from_slice(&slot.to_le_bytes());
    c.push(0xE8);
    let call_end = stub + c.len() as u32 + 4;
    c.extend_from_slice(&target.wrapping_sub(call_end).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0x35]);
    c.extend_from_slice(&slot.to_le_bytes());
    callback(&mut c, idx, exit_cb);
    c.push(0xC3);
    std::ptr::copy_nonoverlapping(c.as_ptr(), stub as *mut u8, c.len());
    *at = (stub + c.len() as u32 + 15) & !15;
    stub
}

/// Install the section wrappers (idempotent; no-op unless `NOITA_ONLINE_PROF` or
/// `NOITA_ONLINE_STEPLOG` is set).
///
/// # Safety
/// Inside noita.exe build 0x6794ee3c, game thread, after the determinism patches.
pub unsafe fn install() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if (!*PROF_ON && STEPLOG.is_none()) || DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let page = VirtualAlloc(std::ptr::null_mut(), 0x4000, 0x3000, 0x40) as u32;
    if page == 0 {
        return;
    }
    let enter_cb = (enter as extern "C" fn(u32)) as u32;
    let exit_cb = (exit as extern "C" fn(u32)) as u32;
    let mut at = page;
    for (i, (_, hook)) in SECTIONS.iter().enumerate() {
        let target = match *hook {
            Hook::Call(site) => {
                if *(site as *const u8) != 0xE8 {
                    continue;
                }
                (site + 5).wrapping_add(*((site + 1) as *const u32))
            }
            Hook::Vtbl(slot) => *(slot as *const u32),
        };
        let stub = wrap(&mut at, i as u32, target, Some(enter_cb), exit_cb);
        match *hook {
            Hook::Call(site) => write(site + 1, &stub.wrapping_sub(site + 5).to_le_bytes()),
            Hook::Vtbl(slot) => write(slot, &stub.to_le_bytes()),
        }
    }
    if std::env::var_os("NOITA_ONLINE_SAMPLE").is_some() {
        // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION
        let thread = OpenThread(0x2 | 0x8 | 0x40, 0, GetCurrentThreadId());
        if thread != 0 {
            std::thread::spawn(move || sampler(thread));
        }
    }
}
