//! Diagnostics: `NOITA_ONLINE_WAKEWATCH=from-to` watches the sleep state of up to four
//! sleeping dynamic Box2D bodies with hardware write watchpoints on the game thread and logs
//! every change of their awake flag (the code that changed it, its EBP-chain return addresses,
//! inside or outside the simulation window) to `$NOITA_ONLINE_OUT/wake_<instance>.txt`.
//! At each window entry in the frame range, the set is re-picked when a watched body is no
//! longer a sleeping body in the world's list: the candidates are taken evenly spread over
//! the body list, so every client watches the same bodies.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc;

type Bool = i32;
extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut u32) -> i32) -> *mut u8;
    fn OpenThread(access: u32, inherit: Bool, id: u32) -> isize;
    fn SuspendThread(thread: isize) -> u32;
    fn ResumeThread(thread: isize) -> u32;
    fn GetThreadContext(thread: isize, ctx: *mut u8) -> Bool;
    fn SetThreadContext(thread: isize, ctx: *const u8) -> Bool;
    fn GetCurrentThreadId() -> u32;
}

/// b2Body: +8 type (2 = dynamic), +0xc flags (u16; 2 = awake, 0x20 = active), +0x18 x,
/// +0x20 y (f64), +0x108 sleep time (f64), +0xb8 next.
const BODY_FLAGS: u32 = 0xc;
const AWAKE: u16 = 2;
const ACTIVE: u16 = 0x20;

static WATCH: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static PREV: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];
static GAME_THREAD: AtomicU32 = AtomicU32::new(0);
static HITS: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Every watchpoint exception on the game thread (any write to a watched flags word).
static RAW: AtomicU32 = AtomicU32::new(0);

struct Log {
    from: u32,
    to: u32,
    file: std::sync::Mutex<std::fs::File>,
    arm: std::sync::Mutex<mpsc::Sender<([u32; 4], mpsc::Sender<bool>)>>,
}

static LOG: std::sync::LazyLock<Option<Log>> = std::sync::LazyLock::new(|| {
    let v = std::env::var("NOITA_ONLINE_WAKEWATCH").ok()?;
    let (a, b) = v.split_once('-')?;
    let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let file = std::fs::File::create(std::path::Path::new(&out).join(format!("wake_{inst}.txt"))).ok()?;
    // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_SET_CONTEXT | THREAD_QUERY_INFORMATION
    let thread = unsafe { OpenThread(0x2 | 0x8 | 0x10 | 0x40, 0, GetCurrentThreadId()) };
    if thread == 0 {
        return None;
    }
    GAME_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel::<([u32; 4], mpsc::Sender<bool>)>();
    std::thread::spawn(move || {
        for (addrs, done) in rx {
            let ok = unsafe { set_debug_registers(thread, &addrs) };
            let _ = done.send(ok);
        }
    });
    unsafe { AddVectoredExceptionHandler(1, handler) };
    Some(Log { from: a.parse().ok()?, to: b.parse().ok()?, file: std::sync::Mutex::new(file), arm: std::sync::Mutex::new(tx) })
});

/// x86 CONTEXT: flags +0, Dr0-3 +4..+0x10, Dr6 +0x14, Dr7 +0x18, Ebp +0xb4, Eip +0xb8,
/// Esp +0xc4; 0x2cc bytes.
#[repr(C, align(16))]
struct Context([u8; 0x2d0]);

unsafe fn set_debug_registers(thread: isize, addrs: &[u32; 4]) -> bool {
    let mut ctx = Context([0; 0x2d0]);
    let p = ctx.0.as_mut_ptr();
    *(p as *mut u32) = 0x0001_0010; // CONTEXT_i386 | CONTEXT_DEBUG_REGISTERS
    if SuspendThread(thread) == u32::MAX {
        return false;
    }
    let mut ok = GetThreadContext(thread, p) != 0;
    if ok {
        let mut dr7 = 0u32;
        for (i, &a) in addrs.iter().enumerate() {
            *(p.add(4 + 4 * i) as *mut u32) = a;
            if a != 0 {
                // local enable, RW = 01 (data writes), LEN = 01 (2 bytes)
                dr7 |= (1 << (2 * i)) | (1 << (16 + 4 * i)) | (1 << (18 + 4 * i));
            }
        }
        *(p.add(0x14) as *mut u32) = 0;
        *(p.add(0x18) as *mut u32) = dr7;
        *(p as *mut u32) = 0x0001_0010;
        ok = SetThreadContext(thread, p) != 0;
    }
    ResumeThread(thread);
    ok
}

unsafe fn rd(a: u32) -> u32 {
    *(a as *const u32)
}

unsafe fn frame() -> u32 {
    let gg = rd(crate::patches::GAME_GLOBAL_PTR);
    if gg != 0 {
        rd(gg)
    } else {
        0
    }
}

pub(crate) fn write_line(line: &str) {
    use std::io::Write;
    if let Some(log) = LOG.as_ref() {
        if let Ok(mut f) = log.file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

unsafe extern "system" fn handler(info: *mut u32) -> i32 {
    const CONTINUE_SEARCH: i32 = 0;
    const CONTINUE_EXECUTION: i32 = -1;
    let rec = *info as *const u32;
    let ctx = *info.add(1) as *mut u8;
    if *rec != 0x8000_0004 || GetCurrentThreadId() != GAME_THREAD.load(Ordering::Relaxed) {
        return CONTINUE_SEARCH;
    }
    RAW.fetch_add(1, Ordering::Relaxed);
    let mut ours = false;
    for i in 0..4 {
        let body = WATCH[i].load(Ordering::Relaxed);
        if body == 0 {
            continue;
        }
        ours = true;
        let flags = *((body + BODY_FLAGS) as *const u16);
        let awake = (flags & AWAKE != 0) as u8;
        if PREV[i].swap(awake, Ordering::Relaxed) == awake {
            continue;
        }
        HITS.fetch_add(1, Ordering::Relaxed);
        let r = |o: usize| *(ctx.add(o) as *const u32);
        let (eip, mut ebp, esp) = (r(0xb8), r(0xb4), r(0xc4));
        let mut chain = format!("{eip:08x}");
        for _ in 0..14 {
            if ebp < esp || ebp > esp.wrapping_add(0x10_0000) || ebp & 3 != 0 {
                break;
            }
            let ret = rd(ebp + 4);
            chain.push_str(&format!(" {ret:08x}"));
            ebp = rd(ebp);
        }
        let x = *((body + 0x18) as *const f64);
        let y = *((body + 0x20) as *const f64);
        write_line(&format!(
            "{} {} w{i} body={body:08x} at {x:.4},{y:.4} awake={awake} flags={flags:04x} chain {chain}",
            frame(),
            if crate::patches::in_window() { "in" } else { "out" },
        ));
    }
    if ours {
        CONTINUE_EXECUTION
    } else {
        CONTINUE_SEARCH
    }
}

/// Window entry (game thread): re-pick and arm the watched bodies when needed.
///
/// # Safety
/// Game thread, world initialised.
pub unsafe fn on_window_entry(b2_world: u32) {
    let Some(log) = LOG.as_ref() else { return };
    let f = frame();
    if f < log.from || f > log.to || b2_world == 0 {
        if INSTALLED.swap(false, Ordering::Relaxed) {
            arm(log, [0; 4]);
        }
        return;
    }
    // Bodies in list order; a watched body is still fine while it is listed and asleep.
    let mut bodies = Vec::new();
    let mut b = rd(b2_world + 0x1922c);
    while b != 0 && bodies.len() < 100_000 {
        bodies.push(b);
        b = rd(b + 0xb8);
    }
    let sleeping = |b: u32| {
        let flags = *((b + BODY_FLAGS) as *const u16);
        rd(b + 8) == 2 && flags & AWAKE == 0 && flags & ACTIVE != 0
    };
    let cands: Vec<(usize, u32)> = bodies.iter().copied().enumerate().filter(|&(_, b)| sleeping(b)).collect();
    let current: Vec<u32> = WATCH.iter().map(|w| w.load(Ordering::Relaxed)).filter(|&w| w != 0).collect();
    let fine = INSTALLED.load(Ordering::Relaxed)
        && current.len() >= cands.len().min(4)
        && current.iter().all(|&w| bodies.contains(&w) && sleeping(w));
    if fine {
        if f % 600 == 0 {
            write_line(&format!("{f} status raw={} hits={} sleeping={}", RAW.load(Ordering::Relaxed), HITS.load(Ordering::Relaxed), cands.len()));
        }
        return;
    }
    let mut pick = [0u32; 4];
    let mut desc = String::new();
    let n = cands.len().min(4);
    for k in 0..n {
        let (idx, body) = cands[k * cands.len() / n];
        pick[k] = body;
        desc.push_str(&format!(" w{k}=#{idx}@{body:08x}"));
    }
    // Disarm first so the handler does not see half-updated slots.
    for w in &WATCH {
        w.store(0, Ordering::Relaxed);
    }
    for (k, &body) in pick.iter().enumerate() {
        if body != 0 {
            PREV[k].store(0, Ordering::Relaxed);
        }
        WATCH[k].store(body, Ordering::Relaxed);
    }
    let ok = arm(log, pick);
    INSTALLED.store(true, Ordering::Relaxed);
    write_line(&format!(
        "{f} arm ok={ok} bodies={} sleeping={} hits={} raw={}{desc}",
        bodies.len(),
        cands.len(),
        HITS.load(Ordering::Relaxed),
        RAW.load(Ordering::Relaxed)
    ));
}

fn arm(log: &Log, addrs: [u32; 4]) -> bool {
    let mut targets = [0u32; 4];
    for (t, a) in targets.iter_mut().zip(addrs) {
        if a != 0 {
            *t = a + BODY_FLAGS;
        }
    }
    let (tx, rx) = mpsc::channel();
    let Ok(sender) = log.arm.lock() else { return false };
    if sender.send((targets, tx)).is_err() {
        return false;
    }
    drop(sender);
    rx.recv().unwrap_or(false)
}
