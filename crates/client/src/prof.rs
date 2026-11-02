//! Diagnostics: frame-section profiler (`NOITA_ONLINE_PROF=1`). Wraps top-level engine calls
//! of the frame (DeathMatch::Update and its children, GridWorld::Update's steps, the mod Lua
//! dispatches, the world draw) and writes their inclusive main-thread time per frame every
//! 600 frames to `$NOITA_ONLINE_OUT/sections_<instance>.txt`. Works with and without the
//! determinism patches (install after them: wrappers call whatever the site calls now).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
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
    ("swap_window (SDL_GL_SwapWindow wrapper)", Hook::Vtbl(0x0105_3238)),
];
const DM_UPDATE: usize = 0;
const WORLD: usize = 4;
const AFTER_ENTITIES: usize = 19;
const N: usize = SECTIONS.len();

struct State {
    start: [Option<Instant>; N],
    last_exit: [Option<Instant>; N],
    total: [Duration; N],
    calls: [u64; N],
    entities: Duration,
    frames: u32,
    window_start: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    start: [None; N],
    last_exit: [None; N],
    total: [Duration::ZERO; N],
    calls: [0; N],
    entities: Duration::ZERO,
    frames: 0,
    window_start: None,
});

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
}

extern "C" fn exit(i: u32) {
    let now = Instant::now();
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let i = i as usize;
    if let Some(t) = s.start[i].take() {
        s.total[i] += now.duration_since(t);
        s.calls[i] += 1;
    }
    s.last_exit[i] = Some(now);
    if i != DM_UPDATE {
        return;
    }
    s.frames += 1;
    if s.frames < 600 {
        return;
    }
    let wall = s.window_start.map_or(Duration::ZERO, |t| now.duration_since(t));
    let n = s.frames as f64;
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / n;
    let wait = Duration::from_micros(WAIT_US.swap(0, Ordering::Relaxed));
    let mut out = format!("{} frames: wall {:.2} ms/frame, tick wait {:.2}, entities (world exit -> marker) {:.2}\n", s.frames, per(wall), per(wait), per(s.entities));
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
        use std::io::Write;
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

/// Install the section wrappers (idempotent; no-op unless `NOITA_ONLINE_PROF` is set).
///
/// # Safety
/// Inside noita.exe build 0x6794ee3c, game thread, after the determinism patches.
pub unsafe fn install() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if std::env::var_os("NOITA_ONLINE_PROF").is_none() || DONE.swap(true, Ordering::Relaxed) {
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
        // Return-address slot (sites are not re-entered), then the stub:
        // enter; pop [slot]; call target; push [slot]; exit; ret
        let slot = at;
        let stub = at + 4;
        let mut c = Vec::with_capacity(256);
        callback(&mut c, i as u32, enter_cb);
        c.extend_from_slice(&[0x8F, 0x05]);
        c.extend_from_slice(&slot.to_le_bytes());
        c.push(0xE8);
        let call_end = stub + c.len() as u32 + 4;
        c.extend_from_slice(&target.wrapping_sub(call_end).to_le_bytes());
        c.extend_from_slice(&[0xFF, 0x35]);
        c.extend_from_slice(&slot.to_le_bytes());
        callback(&mut c, i as u32, exit_cb);
        c.push(0xC3);
        std::ptr::copy_nonoverlapping(c.as_ptr(), stub as *mut u8, c.len());
        at = (stub + c.len() as u32 + 15) & !15;
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
