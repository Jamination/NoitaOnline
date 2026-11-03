//! Diagnostics: `NOITA_ONLINE_RNGWATCH=from-to` finds simulation RNG draws on threads other
//! than the game thread. In that frame range, every simulation window arms hardware write
//! watchpoints on the global LCG, the particle RNG, the GridWorld RNG and the `rand` state
//! in every other thread of the process (disarmed at window exit, where the threaded grid
//! render legitimately draws the render stream). Each write is logged with the thread, the
//! writing code and its EBP-chain return addresses to `$NOITA_ONLINE_OUT/rngwatch_<instance>.txt`
//! (the first 20 per writing address; the counts per address every 600 frames).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

type Bool = i32;
extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut u32) -> i32) -> *mut u8;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Thread32First(snapshot: isize, entry: *mut u32) -> Bool;
    fn Thread32Next(snapshot: isize, entry: *mut u32) -> Bool;
    fn CloseHandle(h: isize) -> Bool;
    fn OpenThread(access: u32, inherit: Bool, id: u32) -> isize;
    fn SuspendThread(thread: isize) -> u32;
    fn ResumeThread(thread: isize) -> u32;
    fn GetThreadContext(thread: isize, ctx: *mut u8) -> Bool;
    fn SetThreadContext(thread: isize, ctx: *const u8) -> Bool;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
}

const NAMES: [&str; 5] = ["lcg", "part", "gw", "rand", "any"];

struct Log {
    from: u32,
    to: u32,
    file: Mutex<std::fs::File>,
    /// Writes per (watch, code address).
    counts: Mutex<HashMap<(usize, u32), u32>>,
    /// The positive-control thread's trigger and acknowledgement (used once).
    #[allow(clippy::type_complexity)]
    control: Mutex<Option<(std::sync::mpsc::Sender<u32>, std::sync::mpsc::Receiver<()>)>>,
}

static LOG: std::sync::LazyLock<Option<Log>> = std::sync::LazyLock::new(|| {
    let v = std::env::var("NOITA_ONLINE_RNGWATCH").ok()?;
    let (a, b) = v.split_once('-')?;
    let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let file = std::fs::File::create(std::path::Path::new(&out).join(format!("rngwatch_{inst}.txt"))).ok()?;
    GAME_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    unsafe { AddVectoredExceptionHandler(1, handler) };
    // Positive control: a thread that rewrites the watched global LCG (same value, while the
    // game thread waits) when the first armed window asks it to, so the log shows one `lcg`
    // hit from it.
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Ok(addr) = rx.recv() {
            unsafe { std::ptr::write_volatile(addr as *mut u32, std::ptr::read_volatile(addr as *const u32)) };
            let _ = ack_tx.send(());
        }
    });
    Some(Log {
        from: a.parse().ok()?,
        to: b.parse().ok()?,
        file: Mutex::new(file),
        counts: Mutex::new(HashMap::new()),
        control: Mutex::new(Some((tx, ack_rx))),
    })
});

static GAME_THREAD: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static FRAME: AtomicU32 = AtomicU32::new(0);

fn write_line(log: &Log, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = log.file.lock() {
        let _ = writeln!(f, "{line}");
    }
}

/// x86 CONTEXT: flags +0, Dr0-3 +4..+0x10, Dr6 +0x14, Dr7 +0x18, Ebp +0xb4, Eip +0xb8,
/// Esp +0xc4; 0x2cc bytes.
#[repr(C, align(16))]
struct Context([u8; 0x2d0]);

/// Set Dr0-3 = `watch` (address, length 1/2/4/8; address 0 = off) in every thread of this
/// process but the game thread. Returns the number of threads set.
unsafe fn set_all(watch: &[(u32, u32); 4]) -> u32 {
    let snap = CreateToolhelp32Snapshot(0x4, 0); // TH32CS_SNAPTHREAD
    if snap == -1 || snap == 0 {
        return 0;
    }
    let (pid, game) = (GetCurrentProcessId(), GAME_THREAD.load(Ordering::Relaxed));
    let mut dr7 = 0u32;
    for (i, &(a, len)) in watch.iter().enumerate() {
        if a != 0 {
            let len_bits = match len {
                1 => 0,
                2 => 1,
                8 => 2,
                _ => 3,
            };
            // local enable, RW = 01 (data writes), LEN
            dr7 |= (1 << (2 * i)) | (1 << (16 + 4 * i)) | (len_bits << (18 + 4 * i));
        }
    }
    let mut n = 0;
    // THREADENTRY32: dwSize, cntUsage, th32ThreadID, th32OwnerProcessID, tpBasePri, tpDeltaPri, dwFlags
    let mut e = [0u32; 7];
    e[0] = 28;
    let mut ok = Thread32First(snap, e.as_mut_ptr()) != 0;
    while ok {
        if e[3] == pid && e[2] != game {
            // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_SET_CONTEXT | THREAD_QUERY_INFORMATION
            let t = OpenThread(0x2 | 0x8 | 0x10 | 0x40, 0, e[2]);
            if t != 0 {
                if SuspendThread(t) != u32::MAX {
                    let mut ctx = Context([0; 0x2d0]);
                    let p = ctx.0.as_mut_ptr();
                    *(p as *mut u32) = 0x0001_0010; // CONTEXT_i386 | CONTEXT_DEBUG_REGISTERS
                    if GetThreadContext(t, p) != 0 {
                        for (i, &(a, _)) in watch.iter().enumerate() {
                            *(p.add(4 + 4 * i) as *mut u32) = a;
                        }
                        *(p.add(0x14) as *mut u32) = 0;
                        *(p.add(0x18) as *mut u32) = dr7;
                        *(p as *mut u32) = 0x0001_0010;
                        if SetThreadContext(t, p) != 0 {
                            n += 1;
                        }
                    }
                    ResumeThread(t);
                }
                CloseHandle(t);
            }
        }
        e[0] = 28;
        ok = Thread32Next(snap, e.as_mut_ptr()) != 0;
    }
    CloseHandle(snap);
    n
}

unsafe extern "system" fn handler(info: *mut u32) -> i32 {
    const CONTINUE_SEARCH: i32 = 0;
    const CONTINUE_EXECUTION: i32 = -1;
    let rec = *info as *const u32;
    let ctx = *info.add(1) as *mut u8;
    if *rec != 0x8000_0004 || GetCurrentThreadId() == GAME_THREAD.load(Ordering::Relaxed) {
        return CONTINUE_SEARCH;
    }
    let Some(log) = LOG.as_ref() else { return CONTINUE_SEARCH };
    // Wine may not report Dr6 in the exception context: then the hit is logged as `any`.
    let dr6 = *(ctx.add(0x14) as *const u32) & 0xf;
    *(ctx.add(0x14) as *mut u32) = 0;
    let hits = if dr6 == 0 { 1 << 4 } else { dr6 };
    let r = |o: usize| *(ctx.add(o) as *const u32);
    let (eip, mut ebp, esp) = (r(0xb8), r(0xb4), r(0xc4));
    for i in 0..5 {
        if hits & (1 << i) == 0 {
            continue;
        }
        let n = {
            let mut c = log.counts.lock().unwrap_or_else(|p| p.into_inner());
            let n = c.entry((i, eip)).or_insert(0);
            *n += 1;
            *n
        };
        if n <= 20 {
            let mut chain = format!("{eip:08x}");
            for _ in 0..16 {
                if ebp < esp || ebp > esp.wrapping_add(0x10_0000) || ebp & 3 != 0 {
                    break;
                }
                chain.push_str(&format!(" {:08x}", *((ebp + 4) as *const u32)));
                ebp = *(ebp as *const u32);
            }
            write_line(
                log,
                &format!(
                    "{} {} thread={} {} chain {chain}",
                    FRAME.load(Ordering::Relaxed),
                    if crate::patches::in_window() { "in" } else { "out" },
                    GetCurrentThreadId(),
                    NAMES[i]
                ),
            );
        }
    }
    CONTINUE_EXECUTION
}

/// Window entry (`enter`) or exit (game thread): arm `watch` (global LCG, particle RNG,
/// GridWorld RNG, `rand` state: (address, length)) in every other thread inside the frame
/// range, disarm at exit.
///
/// # Safety
/// Game thread.
pub unsafe fn on_window(enter: bool, frame: u32, watch: [(u32, u32); 4]) {
    let Some(log) = LOG.as_ref() else { return };
    if enter && frame >= log.from && frame <= log.to {
        FRAME.store(frame, Ordering::Relaxed);
        let n = set_all(&watch);
        ARMED.store(true, Ordering::Relaxed);
        if let Some((go, ack)) = log.control.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if go.send(watch[0].0).is_ok() {
                let _ = ack.recv_timeout(std::time::Duration::from_secs(2));
            }
            write_line(log, &format!("{frame} armed threads={n} (positive control written)"));
        }
        if frame % 600 == 0 {
            let c = log.counts.lock().unwrap_or_else(|p| p.into_inner());
            let mut v: Vec<_> = c.iter().map(|(&(i, eip), &n)| (n, i, eip)).collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            let list: Vec<String> = v.iter().take(40).map(|(n, i, eip)| format!("{}@{eip:08x}={n}", NAMES[*i])).collect();
            drop(c);
            write_line(log, &format!("{frame} status threads={n} counts {}", list.join(" ")));
        }
    } else if !enter && ARMED.swap(false, Ordering::Relaxed) {
        set_all(&[(0, 0); 4]);
    }
}
