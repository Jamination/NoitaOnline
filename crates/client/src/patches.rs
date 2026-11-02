//! In-memory patches that make Noita's simulation deterministic, so every lockstep client
//! computes bit-identical worlds from identical inputs. Nothing on disk is modified; each
//! patch first verifies the original bytes, so a different game build fails loudly instead
//! of corrupting memory.
//!
//! Root causes and evidence: see docs/determinism.md (RE notes for build 0x6794ee3c).

use std::sync::atomic::{AtomicU32, Ordering};

type Bool = i32;
extern "system" {
    fn VirtualProtect(addr: *mut u8, size: usize, new: u32, old: *mut u32) -> Bool;
    fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
    fn FlushInstructionCache(process: isize, addr: *const u8, size: usize) -> Bool;
    fn GetCurrentProcess() -> isize;
    fn Sleep(ms: u32);
}

const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const MEM_COMMIT_RESERVE: u32 = 0x3000;

// --- build 0x6794ee3c facts -------------------------------------------------------------
pub const GAME_GLOBAL_PTR: u32 = 0x0122_374c;
const IAT_RAND: u32 = 0x00f0_7450;
const IAT_SRAND: u32 = 0x00f0_73d4;
/// PlatformDesktop object; +0xa0 LCG state (seeds every grid tile RNG), +0xa4 second seed.
const PLATFORM: u32 = 0x0122_1bc0;
/// Global LCG (x*0x343fd+0x269ec3) used by 100+ engine functions.
const GLOBAL_LCG: u32 = 0x0120_741c;
/// Global Park–Miller generator state (double).
const GLOBAL_PM: u32 = 0x0120_7420;
/// Streaming throttle (FUN_00743990) calls platform->GetTime() (`mov eax,[eax+0x6c]; call eax`).
const STREAM_CLOCK_SITES: [u32; 3] = [0x0074_3a47, 0x0074_3a92, 0x0074_3ab7];
/// `call FUN_0073d6a0` (integrate finished chunk-generation jobs) inside GridWorld::Update.
const CHUNK_INTEGRATE_CALL: u32 = 0x0071_a825;
const CHUNK_INTEGRATE_FN: u32 = 0x0073_d6a0;
/// std::map of pending chunk jobs (job = node+0x24, finished when job+0xc8 == 1).
const CHUNK_JOB_MAP: u32 = 0x0120_8008;
/// Job pool: FUN_0084c240 group submit (JZ selecting threaded path) and FUN_0084c1a0 async submit.
const POOL_GROUP_JZ: u32 = 0x0084_c255;
const POOL_ASYNC_SITE: u32 = 0x0084_c1e0;
const POOL_SLOT_RUNNER: u32 = 0x0084_c430;

#[derive(Debug)]
pub struct PatchError(pub String);

unsafe fn write_code(addr: u32, bytes: &[u8]) -> Result<(), PatchError> {
    let p = addr as *mut u8;
    let mut old = 0u32;
    if VirtualProtect(p, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        return Err(PatchError(format!("VirtualProtect failed at {addr:08x}")));
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    VirtualProtect(p, bytes.len(), old, &mut old);
    FlushInstructionCache(GetCurrentProcess(), p, bytes.len());
    Ok(())
}

unsafe fn expect(addr: u32, bytes: &[u8]) -> Result<(), PatchError> {
    let have = std::slice::from_raw_parts(addr as *const u8, bytes.len());
    if have != bytes {
        return Err(PatchError(format!("unexpected bytes at {addr:08x}: {have:02x?} (want {bytes:02x?}); unsupported Noita build?")));
    }
    Ok(())
}

fn rel32(from_end: u32, to: u32) -> [u8; 4] {
    to.wrapping_sub(from_end).to_le_bytes()
}

/// Executable scratch page holding our stubs and their data.
struct Exec {
    base: u32,
    used: u32,
}

impl Exec {
    unsafe fn new() -> Result<Self, PatchError> {
        let p = VirtualAlloc(std::ptr::null_mut(), 4096, MEM_COMMIT_RESERVE, PAGE_EXECUTE_READWRITE);
        if p.is_null() {
            return Err(PatchError("VirtualAlloc failed".into()));
        }
        Ok(Self { base: p as u32, used: 0 })
    }
    unsafe fn put(&mut self, bytes: &[u8]) -> u32 {
        let at = self.base + self.used;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len());
        self.used = (self.used + bytes.len() as u32 + 15) & !15;
        at
    }
    unsafe fn alloc_data(&mut self, size: u32) -> u32 {
        let at = self.base + self.used;
        std::ptr::write_bytes(at as *mut u8, 0, size as usize);
        self.used = (self.used + size + 15) & !15;
        at
    }
}

struct Installed {
    rand_state: u32,
}

static RAND_STATE_ADDR: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_WAITS: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_WAIT_MS: AtomicU32 = AtomicU32::new(0);

/// Replace msvcr120 rand/srand (per-thread state) with one process-wide LCG, so the grid's
/// per-frame std::random_shuffle calls see the same sequence on every client.
unsafe fn patch_rand(ex: &mut Exec) -> Result<Installed, PatchError> {
    let state = ex.alloc_data(8);
    let s = state.to_le_bytes();
    // mov eax,[state]; imul eax,eax,0x343fd; add eax,0x269ec3; mov [state],eax;
    // shr eax,16; and eax,0x7fff; ret
    let mut rand = vec![0xA1, s[0], s[1], s[2], s[3], 0x69, 0xC0, 0xFD, 0x43, 0x03, 0x00, 0x05, 0xC3, 0x9E, 0x26, 0x00, 0xA3];
    rand.extend_from_slice(&s);
    rand.extend_from_slice(&[0xC1, 0xE8, 0x10, 0x25, 0xFF, 0x7F, 0x00, 0x00, 0xC3]);
    let rand_at = ex.put(&rand);
    // srand: ignore the argument (time-based seeding must not leak in).
    let srand_at = ex.put(&[0xC3]);
    write_code(IAT_RAND, &rand_at.to_le_bytes())?;
    write_code(IAT_SRAND, &srand_at.to_le_bytes())?;
    RAND_STATE_ADDR.store(state, Ordering::Relaxed);
    Ok(Installed { rand_state: state })
}

/// Streaming throttle runs every STREAMING_FREQUENCY seconds of *wall-clock* time; give it
/// game time (frame_num / 60) instead.
unsafe fn patch_stream_clock(ex: &mut Exec) -> Result<(), PatchError> {
    let inv60 = ex.alloc_data(8);
    *(inv60 as *mut f64) = 1.0 / 60.0;
    let g = GAME_GLOBAL_PTR.to_le_bytes();
    let i = inv60.to_le_bytes();
    // mov eax,[GG]; test eax,eax; jz +9; fild dword [eax]; fmul qword [inv60]; ret; fldz; ret
    let stub = [0xA1, g[0], g[1], g[2], g[3], 0x85, 0xC0, 0x74, 0x09, 0xDB, 0x00, 0xDC, 0x0D, i[0], i[1], i[2], i[3], 0xC3, 0xD9, 0xEE, 0xC3];
    let at = ex.put(&stub);
    for site in STREAM_CLOCK_SITES {
        expect(site, &[0x8B, 0x40, 0x6C, 0xFF, 0xD0])?;
    }
    for site in STREAM_CLOCK_SITES {
        let mut call = vec![0xE8];
        call.extend_from_slice(&rel32(site + 5, at));
        write_code(site, &call)?;
    }
    Ok(())
}

unsafe fn rd(a: u32) -> u32 {
    *(a as *const u32)
}

/// Number of chunk-generation jobs not yet finished (walks the MSVC std::map red-black tree).
unsafe fn chunk_jobs_pending() -> u32 {
    let head = rd(CHUNK_JOB_MAP);
    if head == 0 {
        return 0;
    }
    let mut pending = 0;
    let mut stack = vec![rd(head + 4)];
    while let Some(n) = stack.pop() {
        if n == 0 || n == head || *((n + 0xd) as *const u8) != 0 {
            continue;
        }
        let job = rd(n + 0x24);
        if job != 0 && *((job + 0xc8) as *const i32) != 1 {
            pending += 1;
        }
        stack.push(rd(n));
        stack.push(rd(n + 8));
    }
    pending
}

/// Called right before the engine integrates finished chunk jobs: wait until all pending
/// jobs are finished, so chunks always appear in the frame they were requested.
extern "C" fn chunk_wait_barrier() {
    unsafe {
        if chunk_jobs_pending() == 0 {
            return;
        }
        let t0 = std::time::Instant::now();
        while chunk_jobs_pending() > 0 && t0.elapsed().as_secs() < 30 {
            Sleep(0);
        }
        CHUNK_WAITS.fetch_add(1, Ordering::Relaxed);
        CHUNK_WAIT_MS.fetch_add(t0.elapsed().as_millis() as u32, Ordering::Relaxed);
    }
}

unsafe fn patch_chunk_wait(ex: &mut Exec) -> Result<(), PatchError> {
    let orig_rel = rel32(CHUNK_INTEGRATE_CALL + 5, CHUNK_INTEGRATE_FN);
    expect(CHUNK_INTEGRATE_CALL, &[0xE8, orig_rel[0], orig_rel[1], orig_rel[2], orig_rel[3]])?;
    let cb = (chunk_wait_barrier as extern "C" fn()) as u32;
    let c = cb.to_le_bytes();
    let stub_at = ex.base + ex.used;
    // push ecx; mov eax,cb; call eax; pop ecx; jmp FUN_0073d6a0
    let mut stub = vec![0x51, 0xB8, c[0], c[1], c[2], c[3], 0xFF, 0xD0, 0x59, 0xE9];
    stub.extend_from_slice(&rel32(stub_at + 14, CHUNK_INTEGRATE_FN));
    let at = ex.put(&stub);
    debug_assert_eq!(at, stub_at);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(CHUNK_INTEGRATE_CALL + 5, at));
    write_code(CHUNK_INTEGRATE_CALL, &call)
}

/// Run every engine thread-pool job synchronously on the submitting thread. Pool jobs
/// (particles, Box2D step, lighting, world generation) otherwise race on shared RNG state.
unsafe fn patch_inline_jobs() -> Result<(), PatchError> {
    expect(POOL_GROUP_JZ - 4, &[0x80, 0x7F, 0x25, 0x00, 0x74, 0x20])?;
    expect(POOL_ASYNC_SITE, &[0x8D, 0x45, 0xF4, 0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x50, 0x8D, 0x45, 0xEC, 0x89, 0x7B, 0x20])?;
    write_code(POOL_GROUP_JZ, &[0x90, 0x90])?;
    // mov word [ebx+0x24],0; mov [ebx+0x20],edi; push ebx; call slot_runner; add esp,4;
    // mov eax,ebx; pop edi; pop esi; pop ebx; mov esp,ebp; pop ebp; ret 4
    let mut code = vec![0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x89, 0x7B, 0x20, 0x53, 0xE8];
    let call_end = POOL_ASYNC_SITE + code.len() as u32 + 4;
    code.extend_from_slice(&rel32(call_end, POOL_SLOT_RUNNER));
    code.extend_from_slice(&[0x83, 0xC4, 0x04, 0x8B, 0xC3, 0x5F, 0x5E, 0x5B, 0x8B, 0xE5, 0x5D, 0xC2, 0x04, 0x00]);
    write_code(POOL_ASYNC_SITE, &code)
}

static APPLIED: AtomicU32 = AtomicU32::new(0);

/// Install all determinism patches (idempotent). Returns the applied-set bitmask.
///
/// # Safety
/// Must run inside noita.exe build 0x6794ee3c, on the game thread, before the world exists.
pub unsafe fn apply_all() -> Result<u32, PatchError> {
    if APPLIED.load(Ordering::Relaxed) != 0 {
        return Ok(APPLIED.load(Ordering::Relaxed));
    }
    let mut ex = Exec::new()?;
    let installed = patch_rand(&mut ex)?;
    let _ = installed.rand_state;
    patch_stream_clock(&mut ex)?;
    patch_chunk_wait(&mut ex)?;
    patch_inline_jobs()?;
    APPLIED.store(0b1111, Ordering::Relaxed);
    Ok(0b1111)
}

/// Overwrite every time/pid-seeded engine RNG with values derived from the session seed.
/// Call when the world seed is known and again when the world is (re)initialised.
///
/// # Safety
/// Must run inside noita.exe build 0x6794ee3c on the game thread.
pub unsafe fn fix_seeds(seed: u32) {
    *((PLATFORM + 0xa0) as *mut u32) = 0x0012_d687 ^ seed.rotate_left(13);
    *((PLATFORM + 0xa4) as *mut u32) = seed;
    *(GLOBAL_LCG as *mut u32) = seed ^ 0xc;
    *(GLOBAL_PM as *mut f64) = seed as f64;
    let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
    if rs != 0 {
        *(rs as *mut u32) = seed.wrapping_mul(0x9e37_79b9) | 1;
    }
}
