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
    fn GetCurrentThreadId() -> u32;
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

pub(crate) unsafe fn write_code(addr: u32, bytes: &[u8]) -> Result<(), PatchError> {
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

pub(crate) unsafe fn expect(addr: u32, bytes: &[u8]) -> Result<(), PatchError> {
    let have = std::slice::from_raw_parts(addr as *const u8, bytes.len());
    if have != bytes {
        return Err(PatchError(format!("unexpected bytes at {addr:08x}: {have:02x?} (want {bytes:02x?}); unsupported Noita build?")));
    }
    Ok(())
}

pub(crate) fn rel32(from_end: u32, to: u32) -> [u8; 4] {
    to.wrapping_sub(from_end).to_le_bytes()
}

const EXEC_SIZE: usize = 0x10000;

/// Executable scratch area holding our stubs and their data.
pub(crate) struct Exec {
    base: u32,
    used: u32,
}

impl Exec {
    pub(crate) unsafe fn new() -> Result<Self, PatchError> {
        let p = VirtualAlloc(std::ptr::null_mut(), EXEC_SIZE, MEM_COMMIT_RESERVE, PAGE_EXECUTE_READWRITE);
        if p.is_null() {
            return Err(PatchError("VirtualAlloc failed".into()));
        }
        Ok(Self { base: p as u32, used: 0 })
    }
    pub(crate) unsafe fn put(&mut self, bytes: &[u8]) -> u32 {
        let at = self.base + self.used;
        assert!(self.used as usize + bytes.len() <= EXEC_SIZE, "patch stub area full");
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len());
        self.used = (self.used + bytes.len() as u32 + 15) & !15;
        at
    }
    pub(crate) unsafe fn alloc_data(&mut self, size: u32) -> u32 {
        let at = self.base + self.used;
        assert!(self.used as usize + size as usize <= EXEC_SIZE, "patch stub area full");
        std::ptr::write_bytes(at as *mut u8, 0, size as usize);
        self.used = (self.used + size + 15) & !15;
        at
    }
}

static RAND_STATE_ADDR: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_WAITS: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_WAIT_MS: AtomicU32 = AtomicU32::new(0);

/// Replace msvcr120 rand/srand (per-thread state) with one process-wide LCG, so the grid's
/// per-frame std::random_shuffle calls see the same sequence on every client.
unsafe fn patch_rand(ex: &mut Exec) -> Result<(), PatchError> {
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
    Ok(())
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

pub(crate) unsafe fn rd(a: u32) -> u32 {
    *(a as *const u32)
}

/// Every chunk-generation job in the engine's pending map (walks the MSVC std::map
/// red-black tree): (node key = chunk coordinates, job = node+0x24). Keys, not job
/// pointers, identify jobs: heap addresses differ between clients.
unsafe fn chunk_jobs() -> Vec<([u32; 3], u32)> {
    let head = rd(CHUNK_JOB_MAP);
    let mut jobs = Vec::new();
    if head == 0 {
        return jobs;
    }
    let mut stack = vec![rd(head + 4)];
    while let Some(n) = stack.pop() {
        if n == 0 || n == head || *((n + 0xd) as *const u8) != 0 {
            continue;
        }
        let job = rd(n + 0x24);
        if job != 0 {
            jobs.push(([rd(n + 0x10), rd(n + 0x14), rd(n + 0x18)], job));
        }
        stack.push(rd(n));
        stack.push(rd(n + 8));
    }
    jobs
}

unsafe fn job_done(job: u32) -> bool {
    std::ptr::read_volatile((job + 0xc8) as *const i32) == 1
}

/// A chunk requested in frame f is integrated exactly in frame f + CHUNK_DELAY_FRAMES, so
/// generation (on worker threads) overlaps with that many frames of play instead of
/// stalling the frame that requested it. Integration frames are a function of request
/// frames only, hence identical on every client.
const CHUNK_DELAY_FRAMES: u32 = 8;

struct ChunkGate {
    /// job key -> frame it was first seen in the pending map
    first_seen: std::collections::HashMap<[u32; 3], u32>,
    /// During the gated integration call: the jobs it may integrate (old enough, hence
    /// finished). `None` outside it (e.g. the engine's flush-everything loop, FUN_007420f0).
    allowed: Option<Vec<u32>>,
}

static CHUNK_GATE: std::sync::Mutex<Option<ChunkGate>> = std::sync::Mutex::new(None);

/// Wait (outside any engine lock: call at the start of the frame) until every chunk job
/// that will be integrated this frame has finished. Waiting inside the grid update could
/// deadlock against a generation worker that needs a lock the main thread holds there.
/// Returns the milliseconds spent waiting.
///
/// # Safety
/// Game thread, before the world update of the frame.
pub unsafe fn chunk_prewait() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return 0;
    }
    // The world update after `OnWorldPreUpdate(f)` runs as frame f + 1, and that is the
    // frame [`chunk_gate_pre`] judges a job's age by.
    let frame = rd(gg).wrapping_add(1);
    let jobs = chunk_jobs();
    let g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
    let Some(gate) = g.as_ref() else { return 0 };
    let due: Vec<u32> = jobs
        .iter()
        .filter(|(k, _)| gate.first_seen.get(k).is_some_and(|&s| frame.wrapping_sub(s) >= CHUNK_DELAY_FRAMES))
        .map(|&(_, j)| j)
        .collect();
    drop(g);
    let t0 = std::time::Instant::now();
    let mut waited = false;
    for job in due {
        while !job_done(job) && t0.elapsed().as_secs() < 60 {
            waited = true;
            Sleep(1);
        }
    }
    let ms = t0.elapsed().as_millis() as u32;
    if waited {
        CHUNK_WAITS.fetch_add(1, Ordering::Relaxed);
        CHUNK_WAIT_MS.fetch_add(ms, Ordering::Relaxed);
    }
    ms
}

/// Before the engine integrates finished chunk jobs: jobs old enough are finished (see
/// [`chunk_prewait`]) and are the only ones this frame's integration may take.
extern "C" fn chunk_gate_pre() {
    unsafe {
        let gg = rd(GAME_GLOBAL_PTR);
        let frame = if gg != 0 { rd(gg) } else { 0 };
        let jobs = chunk_jobs();
        let mut g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
        let gate = g.get_or_insert_with(|| ChunkGate { first_seen: Default::default(), allowed: None });
        gate.first_seen.retain(|k, _| jobs.iter().any(|(jk, _)| jk == k));
        let mut allowed = Vec::new();
        for &(key, job) in &jobs {
            let seen = *gate.first_seen.entry(key).or_insert(frame);
            if frame.wrapping_sub(seen) >= CHUNK_DELAY_FRAMES {
                if !job_done(job) {
                    // Not pre-waited (first frames before the mod's frame hook runs):
                    // fall back to a bounded wait here.
                    let t0 = std::time::Instant::now();
                    while !job_done(job) && t0.elapsed().as_secs() < 5 {
                        Sleep(1);
                    }
                    CHUNK_GATE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                    CHUNK_GATE_FALLBACK_MAX_MS.fetch_max(t0.elapsed().as_millis() as u32, Ordering::Relaxed);
                    CHUNK_GATE_FALLBACK_FRAME.store(frame, Ordering::Relaxed);
                    if !job_done(job) {
                        // Integrated whenever it finishes: a timing-dependent frame.
                        CHUNK_GATE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                allowed.push(job);
            }
        }
        gate.allowed = Some(allowed);
    }
}

pub static CHUNK_GATE_FALLBACKS: AtomicU32 = AtomicU32::new(0);
/// The longest in-lock fallback wait (ms), the frame of the last fallback, and the fallbacks
/// whose job was still unfinished after the 5 s bound.
pub static CHUNK_GATE_FALLBACK_MAX_MS: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_GATE_FALLBACK_FRAME: AtomicU32 = AtomicU32::new(0);
pub static CHUNK_GATE_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

/// After integration: integration is ungated again.
extern "C" fn chunk_gate_post() {
    let mut g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(gate) = g.as_mut() {
        gate.allowed = None;
    }
}

/// The integration scan's "job finished" read (`job+0xc8 == 1`), replaced by
/// [`chunk_job_state`]. A young job's state cannot be hidden by writing it: a worker that
/// finishes between the gate and the scan would set it to 1 and the chunk would be
/// integrated early, at a timing-dependent frame on this client only.
const CHUNK_JOB_DONE_READ: u32 = 0x0073_d746;

/// `job+0xc8` as the integration scan sees it: 0 (not finished) for every job the gate does
/// not allow this frame.
extern "C" fn chunk_job_state(job: u32) -> u32 {
    let g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
    match g.as_ref().and_then(|gate| gate.allowed.as_ref()) {
        Some(allowed) if !allowed.contains(&job) => 0,
        _ => unsafe { std::ptr::read_volatile((job + 0xc8) as *const u32) },
    }
}

unsafe fn patch_chunk_wait(ex: &mut Exec) -> Result<(), PatchError> {
    let orig_rel = rel32(CHUNK_INTEGRATE_CALL + 5, CHUNK_INTEGRATE_FN);
    expect(CHUNK_INTEGRATE_CALL, &[0xE8, orig_rel[0], orig_rel[1], orig_rel[2], orig_rel[3]])?;
    // mov eax,[edi+0xc8]; cmp eax,1; jnz (not finished)
    expect(CHUNK_JOB_DONE_READ, &[0x8B, 0x87, 0xC8, 0x00, 0x00, 0x00, 0x83, 0xF8, 0x01, 0x0F, 0x85])?;
    let pre = ((chunk_gate_pre as extern "C" fn()) as u32).to_le_bytes();
    let post = ((chunk_gate_post as extern "C" fn()) as u32).to_le_bytes();
    let stub_at = ex.base + ex.used;
    // push ecx; mov eax,pre; call eax; pop ecx; call FUN_0073d6a0 (fastcall ecx);
    // mov eax,post; call eax; ret
    let mut stub = vec![0x51, 0xB8, pre[0], pre[1], pre[2], pre[3], 0xFF, 0xD0, 0x59, 0xE8];
    stub.extend_from_slice(&rel32(stub_at + 14, CHUNK_INTEGRATE_FN));
    stub.extend_from_slice(&[0xB8, post[0], post[1], post[2], post[3], 0xFF, 0xD0, 0xC3]);
    let at = ex.put(&stub);
    debug_assert_eq!(at, stub_at);
    let state = ((chunk_job_state as extern "C" fn(u32) -> u32) as u32).to_le_bytes();
    // push ecx; push edx; push edi; mov eax,chunk_job_state; call eax; add esp,4; pop edx;
    // pop ecx; cmp eax,1; ret (-> the original jnz)
    let read = ex.put(&[0x51, 0x52, 0x57, 0xB8, state[0], state[1], state[2], state[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x5A, 0x59, 0x83, 0xF8, 0x01, 0xC3]);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(CHUNK_JOB_DONE_READ + 5, read));
    call.extend_from_slice(&[0x90; 4]);
    write_code(CHUNK_JOB_DONE_READ, &call)?;
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(CHUNK_INTEGRATE_CALL + 5, at));
    write_code(CHUNK_INTEGRATE_CALL, &call)
}

/// Group jobs (particles, lighting, ... — everything but the grid tiles, see
/// [`patch_threaded_grid`]) run synchronously on the submitting thread: they otherwise race
/// on shared RNG state.
unsafe fn patch_inline_group() -> Result<(), PatchError> {
    expect(POOL_GROUP_JZ - 4, &[0x80, 0x7F, 0x25, 0x00, 0x74, 0x20])?;
    write_code(POOL_GROUP_JZ, &[0x90, 0x90])
}

/// Async jobs (cosmetic particles, chunk saving, the pathfinding search, ... — everything but
/// chunk generation, see [`patch_threaded_chunkgen`], and the Box2D step, see
/// [`crate::box2d`]) run synchronously at submission: they would otherwise race the simulation.
unsafe fn patch_inline_async() -> Result<(), PatchError> {
    expect(POOL_ASYNC_SITE, &[0x8D, 0x45, 0xF4, 0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x50, 0x8D, 0x45, 0xEC, 0x89, 0x7B, 0x20])?;
    // mov word [ebx+0x24],0; mov [ebx+0x20],edi; push ebx; call slot_runner; add esp,4;
    // mov eax,ebx; pop edi; pop esi; pop ebx; mov esp,ebp; pop ebp; ret 4
    let mut code = vec![0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x89, 0x7B, 0x20, 0x53, 0xE8];
    let call_end = POOL_ASYNC_SITE + code.len() as u32 + 4;
    code.extend_from_slice(&rel32(call_end, POOL_SLOT_RUNNER));
    code.extend_from_slice(&[0x83, 0xC4, 0x04, 0x8B, 0xC3, 0x5F, 0x5E, 0x5B, 0x8B, 0xE5, 0x5D, 0xC2, 0x04, 0x00]);
    write_code(POOL_ASYNC_SITE, &code)
}

/// Async job submit (FUN_0084c1a0): 0xa0 bytes, rel32 calls at +0x2f and +0x5e.
const POOL_ASYNC_FN: u32 = 0x0084_c1a0;
const POOL_ASYNC_LEN: usize = 0xa0;
const POOL_ASYNC_RELCALLS: [usize; 2] = [0x2f, 0x5e];
/// The chunk-generation dispatch's call to the async submit (inside FUN_0073c220).
const CHUNKGEN_SUBMIT_CALL: u32 = 0x0073_c3f1;
/// Cell constructor's `mov [GLOBAL_LCG],eax`: advances the global RNG for burning
/// materials, but its result is dead (the burning flag is set either way). Generation
/// runs it on worker threads, racing the main thread on the global RNG.
const CELL_CTOR_LCG_STORE: u32 = 0x0070_f4cc;

/// Copy an (unpatched) engine function into the stub page, relocating its rel32 calls.
unsafe fn copy_function(ex: &mut Exec, addr: u32, len: usize, relcalls: &[usize]) -> Result<u32, PatchError> {
    let mut body = std::slice::from_raw_parts(addr as *const u8, len).to_vec();
    let copy_at = ex.base + ex.used;
    for &off in relcalls {
        if body[off] != 0xE8 {
            return Err(PatchError(format!("layout of {addr:08x} changed at +{off:#x}")));
        }
        let old_rel = i32::from_le_bytes(body[off + 1..off + 5].try_into().unwrap());
        let target = (addr + off as u32 + 5).wrapping_add(old_rel as u32);
        body[off + 1..off + 5].copy_from_slice(&rel32(copy_at + off as u32 + 5, target));
    }
    let at = ex.put(&body);
    debug_assert_eq!(at, copy_at);
    Ok(at)
}

/// Point the `call` at `site` (currently calling `orig`) at `new_target`.
pub(crate) unsafe fn redirect_call(site: u32, orig: u32, new_target: u32) -> Result<(), PatchError> {
    let orig_rel = rel32(site + 5, orig);
    expect(site, &[0xE8, orig_rel[0], orig_rel[1], orig_rel[2], orig_rel[3]])?;
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(site + 5, new_target));
    write_code(site, &call)
}

/// Keep world generation on the engine's worker threads (it is expensive: 17-36 ms per
/// chunk) while everything else runs inline: chunk jobs get an unpatched copy of the
/// async submit, and their only global-LCG write is removed. Integration stays
/// frame-deterministic through the chunk-wait barrier. Each job runs through
/// [`chunk_job_stub`], which gives it its own cell RNG stream ([`patch_chunk_rng`]) and its
/// submission frame ([`chunk_job_submitted`], [`patch_chunk_frame`]). Must run before
/// [`patch_inline_jobs`] (it copies the original submit routine).
unsafe fn patch_threaded_chunkgen(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CELL_CTOR_LCG_STORE, &[0xA3, 0x1C, 0x74, 0x20, 0x01])?;
    expect(POOL_ASYNC_FN, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x18])?;
    let job_jmp = rel32(CHUNK_JOB_JMP + 5, CHUNK_JOB_FN);
    expect(CHUNK_JOB_JMP, &[0xE9, job_jmp[0], job_jmp[1], job_jmp[2], job_jmp[3]])?;
    let copy = copy_function(ex, POOL_ASYNC_FN, POOL_ASYNC_LEN, &POOL_ASYNC_RELCALLS)?;
    // EDI = the job at the submit call: push ecx; push edi; mov eax,chunk_job_submitted;
    // call eax; add esp,4; pop ecx; jmp copy
    let f = ((chunk_job_submitted as extern "C" fn(u32)) as u32).to_le_bytes();
    let stub_at = ex.base + ex.used;
    let mut stub = vec![0x51, 0x57, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x59, 0xE9];
    stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, copy));
    let submit = ex.put(&stub);
    debug_assert_eq!(submit, stub_at);
    redirect_call(CHUNKGEN_SUBMIT_CALL, POOL_ASYNC_FN, submit)?;
    write_code(CELL_CTOR_LCG_STORE, &[0x90; 5])?;
    let stub = chunk_job_stub(ex);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(CHUNK_JOB_JMP + 5, stub));
    write_code(CHUNK_JOB_JMP, &jmp)
}

/// The chunk job (lambda call `FUN_009a4f50`: `mov ecx,[ecx+4]; jmp FUN_0073b040`) generates
/// a chunk, or loads it from the save, into the job's buffer (`FUN_0073b040`, ECX = job;
/// job+0x58/+0x5c = chunk x, y; job+0x74 = the world it builds cells with).
const CHUNK_JOB_JMP: u32 = 0x009a_4f53;
const CHUNK_JOB_FN: u32 = 0x0073_b040;

thread_local! {
    /// The cell RNG state of the chunk job running on this thread, if any ([`patch_chunk_rng`]).
    static CHUNK_JOB_RNG: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    /// The submission frame of the chunk job running on this thread, if any
    /// ([`patch_chunk_frame`]).
    static CHUNK_JOB_FRAME: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The grid frame counter (the world's vtable+0x10, `[GridWorld+0x4fc]`) at each pending
/// chunk job's submission, by job ([`chunk_job_submitted`]; taken by [`chunk_job_begin`]).
static CHUNK_SUBMIT_FRAMES: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());

/// The world's frame counter (vtable+0x10).
unsafe fn world_frame(world: u32) -> u32 {
    let get: extern "thiscall" fn(u32) -> u32 = std::mem::transmute(rd(rd(world) + 0x10) as usize);
    get(world)
}

/// Game thread, as the chunk-generation dispatch submits `job` (whose world, job+0x74, is
/// set): remember the grid frame.
extern "C" fn chunk_job_submitted(job: u32) {
    let world = unsafe { rd(job + 0x74) };
    if world == 0 {
        return;
    }
    let frame = unsafe { world_frame(world) };
    let mut v = CHUNK_SUBMIT_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
    v.retain(|&(j, _)| j != job);
    v.push((job, frame));
}

/// Session seed (set by [`fix_seeds`]): salt of the chunk jobs' cell RNG streams.
static SESSION_SEED: AtomicU32 = AtomicU32::new(0);

/// Job entry (ECX = job): `push ecx; push ecx; call chunk_job_begin; add esp,4; pop ecx;
/// call FUN_0073b040; call chunk_job_end; ret`.
unsafe fn chunk_job_stub(ex: &mut Exec) -> u32 {
    let begin = ((chunk_job_begin as extern "C" fn(u32)) as u32).to_le_bytes();
    let end = ((chunk_job_end as extern "C" fn()) as u32).to_le_bytes();
    let job = CHUNK_JOB_FN.to_le_bytes();
    ex.put(&[
        0x51, 0x51, 0xB8, begin[0], begin[1], begin[2], begin[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x59, // begin(job)
        0xB8, job[0], job[1], job[2], job[3], 0xFF, 0xD0, // FUN_0073b040(job)
        0xB8, end[0], end[1], end[2], end[3], 0xFF, 0xD0, 0xC3, // end()
    ])
}

/// A chunk job starts on this thread: its cell RNG stream starts from the session seed and
/// the chunk's coordinates, the same on every client and every time the chunk is built; its
/// cells' creation frame is its submission frame.
extern "C" fn chunk_job_begin(job: u32) {
    let (cx, cy) = unsafe { (rd(job + 0x58), rd(job + 0x5c)) };
    let seed = SESSION_SEED.load(Ordering::Relaxed) ^ cx.wrapping_mul(0x9e37_79b1) ^ cy.wrapping_mul(0x85eb_ca77);
    let _ = CHUNK_JOB_RNG.try_with(|r| r.set(Some(seed)));
    let frame = {
        let mut v = CHUNK_SUBMIT_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
        v.iter().position(|&(j, _)| j == job).map(|i| v.swap_remove(i).1)
    };
    let _ = CHUNK_JOB_FRAME.try_with(|f| f.set(frame));
    chunk_job_delay();
}

extern "C" fn chunk_job_end() {
    let _ = CHUNK_JOB_RNG.try_with(|r| r.set(None));
    let _ = CHUNK_JOB_FRAME.try_with(|f| f.set(None));
}

/// Diagnostics: `NOITA_ONLINE_CHUNKJOB_DELAY=<instance>:<ms>` delays the start of every chunk
/// job on that instance by ms, as a stand-in for a slower or busier machine (the chunk-wait
/// barrier keeps their integration on the same frame).
fn chunk_job_delay() {
    static MS: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
        let v = std::env::var("NOITA_ONLINE_CHUNKJOB_DELAY").unwrap_or_default();
        let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
        v.split_once(':').filter(|(i, _)| *i == inst).and_then(|(_, ms)| ms.parse().ok()).unwrap_or(0)
    });
    if *MS > 0 {
        unsafe { Sleep(*MS) };
    }
}

/// Cell constructors' draws on the RNG of the world they are given (`imul eax,[reg+4],0x343fd;
/// add eax,0x269ec3; mov [reg+4],eax`, reg = the world): (site, ModRM of `[reg+4]`, whether
/// `mov ecx,esi; neg ecx; sub esi,ecx; inc esi` sits between the imul and the add).
const CELL_RNG_SITES: [(u32, u8, bool); 4] = [
    (0x0070_a4ee, 0x43, false), // CGasCell (FUN_0070a4a0): rising flag, EBX = world
    (0x0070_a538, 0x43, true),  // CGasCell: lifetime jitter
    (0x0070_9f50, 0x41, false), // CFireCell (FUN_00709ef0): lifetime, ECX = world
    (0x0070_aed7, 0x43, true),  // CLiquidCell (FUN_0070ae10): lifetime jitter
];
const CELL_RNG_BETWEEN: [u8; 7] = [0x8B, 0xCE, 0xF7, 0xD9, 0x2B, 0xF1, 0x46];

/// Chunk jobs build cells with the real GridWorld (job+0x74), so the gas, fire and
/// lifetime-liquid constructors drew the GridWorld RNG (+4) on a worker thread while the
/// simulation drew it on the game thread (cells created by particles, explosions, reactions).
/// Which draws each side got depended on thread timing: loaded and generated gas, fire and
/// liquid cells got client-specific flags and lifetimes (invisible to the material hash until
/// they moved or expired differently), and the simulation's own cells got client-specific
/// values. Now a constructor running inside a chunk job draws that job's own stream
/// ([`chunk_job_begin`]); everywhere else (game thread, grid tiles with their own RNG) it
/// draws the world it is given, as before.
unsafe fn patch_chunk_rng(ex: &mut Exec) -> Result<(), PatchError> {
    let original = |modrm: u8, between: bool| {
        let mut b = vec![0x69, modrm, 0x04, 0xFD, 0x43, 0x03, 0x00];
        if between {
            b.extend_from_slice(&CELL_RNG_BETWEEN);
        }
        b.extend_from_slice(&[0x05, 0xC3, 0x9E, 0x26, 0x00, 0x89, modrm, 0x04]);
        b
    };
    for (site, modrm, between) in CELL_RNG_SITES {
        expect(site, &original(modrm, between))?;
    }
    // EAX = &state -> EAX = the new state; ECX/EDX preserved: push ecx; push edx; push eax;
    // call cell_rng_draw; add esp,4; pop edx; pop ecx; ret
    let f = ((cell_rng_draw as extern "C" fn(*mut u32) -> u32) as u32).to_le_bytes();
    let stub = ex.put(&[0x51, 0x52, 0x50, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x5A, 0x59, 0xC3]);
    for (site, modrm, between) in CELL_RNG_SITES {
        // lea eax,[reg+4]; call stub; (the moved instructions); nop padding
        let mut code = vec![0x8D, modrm, 0x04, 0xE8];
        code.extend_from_slice(&rel32(site + 8, stub));
        if between {
            code.extend_from_slice(&CELL_RNG_BETWEEN);
        }
        code.resize(original(modrm, between).len(), 0x90);
        write_code(site, &code)?;
    }
    Ok(())
}

/// One step of the world RNG at `state`, or of this thread's chunk-job stream while a chunk
/// job runs on it.
extern "C" fn cell_rng_draw(state: *mut u32) -> u32 {
    let step = |s: u32| s.wrapping_mul(0x343fd).wrapping_add(0x269ec3);
    let job = CHUNK_JOB_RNG.try_with(|r| {
        let n = step(r.get()?);
        r.set(Some(n));
        Some(n)
    });
    if let Ok(Some(n)) = job {
        return n;
    }
    unsafe {
        let n = step(*state);
        *state = n;
        n
    }
}

/// The fire and lifetime-liquid constructors read the creation frame through the world's
/// vtable+0x10 (ECX = the world): CFireCell (FUN_00709ef0) `mov eax,[ecx]; mov esi,edx;
/// call [eax+0x10]` (fire age), CLiquidCell (FUN_0070ae10) `mov eax,[ebx]; call [eax+0x10]`
/// (ECX = EBX; liquid lifetime start). Replacement: (site, original, the bytes kept before
/// the call).
const CELL_FRAME_SITES: [(u32, &[u8], &[u8]); 2] = [
    (0x0070_9f6a, &[0x8B, 0x01, 0x8B, 0xF2, 0xFF, 0x50, 0x10], &[0x8B, 0xF2]),
    (0x0070_ae97, &[0x8B, 0x03, 0xFF, 0x50, 0x10], &[]),
];

/// Chunk jobs build cells with the real GridWorld, whose frame counter (+0x4fc) the game
/// thread increments at the start of every grid update. A fire or lifetime liquid cell
/// generated or reloaded on a worker thread therefore got the frame the job happened to
/// reach it in: a creation frame (and so a burn-out or expiry frame) that depended on
/// thread timing. Inside a chunk job the constructors now read the job's submission frame
/// ([`chunk_job_submitted`], game thread); everywhere else the world's counter, as before.
unsafe fn patch_chunk_frame(_: &mut Exec) -> Result<(), PatchError> {
    for (site, orig, _) in CELL_FRAME_SITES {
        expect(site, orig)?;
    }
    let f: unsafe extern "thiscall" fn(u32) -> u32 = cell_frame;
    for (site, orig, keep) in CELL_FRAME_SITES {
        let mut code = keep.to_vec();
        let call_end = site + code.len() as u32 + 5;
        code.push(0xE8);
        code.extend_from_slice(&rel32(call_end, f as usize as u32));
        debug_assert_eq!(code.len(), orig.len());
        write_code(site, &code)?;
    }
    Ok(())
}

/// The creation frame a cell constructor sees: the submission frame of the chunk job running
/// on this thread, else the world's frame counter.
unsafe extern "thiscall" fn cell_frame(world: u32) -> u32 {
    match CHUNK_JOB_FRAME.try_with(|f| f.get()) {
        Ok(Some(frame)) => frame,
        _ => world_frame(world),
    }
}

/// Group job submit (FUN_0084c240): 0xd4 bytes, rel32 calls at +0x64 and +0x92.
const POOL_GROUP_FN: u32 = 0x0084_c240;
const POOL_GROUP_LEN: usize = 0xd4;
const POOL_GROUP_RELCALLS: [usize; 2] = [0x64, 0x92];
/// The falling-sand tile dispatches inside the grid update (FUN_00726340).
const GRID_TILE_SUBMIT_CALLS: [u32; 3] = [0x0072_7d5a, 0x0072_8051, 0x0072_82b5];

/// The falling-sand grid's 64x64 tiles run in parallel deterministically (each tile owns
/// its RNG, queues its world edits for the game thread, and the four checkerboard passes
/// never let tiles race; the explosion path, which did not hold to that, is
/// [`crate::explosions`]), so they keep the worker threads; other pool jobs run inline. Must
/// run before [`patch_inline_jobs`].
unsafe fn patch_threaded_grid(ex: &mut Exec) -> Result<(), PatchError> {
    expect(POOL_GROUP_FN, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14])?;
    let copy = copy_function(ex, POOL_GROUP_FN, POOL_GROUP_LEN, &POOL_GROUP_RELCALLS)?;
    for site in GRID_TILE_SUBMIT_CALLS {
        redirect_call(site, POOL_GROUP_FN, copy)?;
    }
    if tile_reverse_here() {
        patch_tile_reverse(ex, copy)?;
    }
    Ok(())
}

/// The checkerboard pass's pooled tile dispatch (`GRID_TILE_SUBMIT_CALLS[2]`): `call` with
/// ECX = job group and the tile lambda (vtable `0x00ff97dc`, +4 = tile, invoke =
/// `FUN_0072a150(tile)`) as the second stack argument; the callee pops 8 bytes.
const GRID_PASS_SUBMIT_CALL: u32 = 0x0072_82b5;
/// End of a pass's dispatch loop, before the join: `mov edi,[ebp-0x88]` (the job group).
const GRID_PASS_JOIN: u32 = 0x0072_82fb;
const GRID_PASS_JOIN_ORIG: [u8; 6] = [0x8B, 0xBD, 0x78, 0xFF, 0xFF, 0xFF];
/// The tile update (thiscall, ECX = tile, no stack arguments).
const TILE_UPDATE_FN: u32 = 0x0072_a150;

/// Diagnostics: `NOITA_ONLINE_TILE_REVERSE=<instance>,...`: on those instances the pass's
/// pooled tiles are not submitted but run on the game thread at the end of the pass's
/// dispatch, in reverse order (after the tiles the pass runs inline anyway). That is a
/// fixed, very different order of the tiles of one pass, so any dependence of a tile's
/// result on what other tiles of its pass did shows up as a divergence from the other
/// clients on every run, instead of now and then through thread timing.
fn tile_reverse_here() -> bool {
    let (Ok(v), Ok(inst)) = (std::env::var("NOITA_ONLINE_TILE_REVERSE"), std::env::var("NOITA_ONLINE_INSTANCE")) else {
        return false;
    };
    v.split(',').any(|s| s.trim() == inst)
}

static DEFERRED_TILES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// [`GRID_PASS_SUBMIT_CALL`] under `NOITA_ONLINE_TILE_REVERSE`: keep the tile for
/// [`run_deferred_tiles`].
unsafe extern "thiscall" fn defer_tile(_group: u32, _arg: u32, functor: u32) {
    DEFERRED_TILES.lock().unwrap_or_else(|p| p.into_inner()).push(rd(functor + 4));
}

/// [`GRID_PASS_JOIN`] under `NOITA_ONLINE_TILE_REVERSE`: the deferred tiles, last first.
extern "C" fn run_deferred_tiles() {
    let tiles = std::mem::take(&mut *DEFERRED_TILES.lock().unwrap_or_else(|p| p.into_inner()));
    unsafe {
        let update: unsafe extern "thiscall" fn(u32) = std::mem::transmute(TILE_UPDATE_FN as usize);
        for &tile in tiles.iter().rev() {
            update(tile);
        }
    }
}

unsafe fn patch_tile_reverse(ex: &mut Exec, copy: u32) -> Result<(), PatchError> {
    expect(GRID_PASS_JOIN, &GRID_PASS_JOIN_ORIG)?;
    let defer: unsafe extern "thiscall" fn(u32, u32, u32) = defer_tile;
    redirect_call(GRID_PASS_SUBMIT_CALL, copy, defer as usize as u32)?;
    // pushad; mov eax,run_deferred_tiles; call eax; popad; mov edi,[ebp-0x88]; ret
    let f = ((run_deferred_tiles as extern "C" fn()) as u32).to_le_bytes();
    let mut stub = vec![0x60, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x61];
    stub.extend_from_slice(&GRID_PASS_JOIN_ORIG);
    stub.push(0xC3);
    let at = ex.put(&stub);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(GRID_PASS_JOIN + 5, at));
    call.push(0x90);
    write_code(GRID_PASS_JOIN, &call)
}

/// GridWorld::RenderTiles (FUN_00723650, called by FUN_00722ef0 after `OnWorldPostUpdate`,
/// i.e. outside the simulation window): its row-lambda dispatch (joined inside the
/// function) and its per-tile `Function_DrawGrid` dispatch (joined by the world draw,
/// FUN_00723bd0 from FUN_006f66a0).
const RENDER_TILES_SUBMIT_CALLS: [u32; 2] = [0x0072_37d7, 0x0072_3a6e];
/// Job group of the last `Function_DrawGrid` dispatch; pending jobs at group+0x1c.
const DRAW_GRID_GROUP: u32 = 0x0120_7000;
static THREADED_RENDER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Grid rendering (7-10 ms per frame inline in a 2-player session) only reads cells and
/// writes this client's render buffers; its one shared-state write is the global LCG
/// (cell colour noise), which outside the window is the per-client render stream. It keeps
/// the worker threads; [`sim_rng_enter`] joins it before the render stream is stashed, so no
/// job ever overlaps the simulation window. Must run before [`patch_inline_group`].
unsafe fn patch_threaded_render(ex: &mut Exec) -> Result<(), PatchError> {
    let copy = threaded_group_submit(ex)?;
    for site in RENDER_TILES_SUBMIT_CALLS {
        redirect_call(site, POOL_GROUP_FN, copy)?;
    }
    THREADED_RENDER.store(true, Ordering::Relaxed);
    Ok(())
}

/// The pathfinding update FUN_00864590 (called at the end of the grid update, inside the
/// window): its `call FUN_0084c240`, one job per updated grid tile.
const PATHFINDING_SUBMIT_CALL: u32 = 0x0086_470c;

/// The pathfinding update's per-tile jobs (FUN_00864790 -> FUN_00864060, ~1.4 ms per frame
/// inline in a 1-player session) recompute each 4x4-pixel node's flags (node+0xf and its byte
/// at +0x80000) from that block's cell summary only: the grid tiles' rects are disjoint, the
/// jobs draw no RNG and allocate nothing, and FUN_00864590 waits for all of them before
/// anything else runs (the tile jobs are joined before it starts). So they keep the worker
/// threads. (Tried once before and dropped after seed 1717171717 forked at f=1383; that
/// fork was [`patch_free_solids_order`]'s, present without any threading.) Must run before
/// [`patch_inline_group`].
unsafe fn patch_threaded_pathfinding(ex: &mut Exec) -> Result<(), PatchError> {
    let copy = threaded_group_submit(ex)?;
    redirect_call(PATHFINDING_SUBMIT_CALL, POOL_GROUP_FN, copy)
}

/// Unpatched copy of the group submit for [`patch_threaded_render`] and
/// [`patch_threaded_pathfinding`] (taken once, before [`patch_inline_group`]).
unsafe fn threaded_group_submit(ex: &mut Exec) -> Result<u32, PatchError> {
    static COPY: AtomicU32 = AtomicU32::new(0);
    if COPY.load(Ordering::Relaxed) == 0 {
        expect(POOL_GROUP_FN, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14])?;
        expect(POOL_GROUP_JZ - 4, &[0x80, 0x7F, 0x25, 0x00, 0x74, 0x20])?;
        COPY.store(copy_function(ex, POOL_GROUP_FN, POOL_GROUP_LEN, &POOL_GROUP_RELCALLS)?, Ordering::Relaxed);
    }
    Ok(COPY.load(Ordering::Relaxed))
}

/// Wait for the last `Function_DrawGrid` dispatch. Normally the world draw has joined it
/// already; this covers frames whose draw was skipped.
unsafe fn join_render_tiles() {
    if !THREADED_RENDER.load(Ordering::Relaxed) {
        return;
    }
    let group = rd(DRAW_GRID_GROUP);
    if group == 0 {
        return;
    }
    while std::ptr::read_volatile((group + 0x1c) as *const i32) > 0 {
        std::thread::yield_now();
    }
}

/// Number of installed patches + 1 (0 = not installed yet).
static APPLIED: AtomicU32 = AtomicU32::new(0);

/// Install all determinism patches (idempotent). Returns the number of [`PATCHES`] rows
/// installed. For experiments, `NOITA_ONLINE_PATCH_OFF=name,name` skips patches by name
/// (never set in normal play: every patch is needed for determinism).
///
/// # Safety
/// Must run inside noita.exe build 0x6794ee3c, on the game thread, before the world exists.
pub unsafe fn apply_all() -> Result<u32, PatchError> {
    let done = APPLIED.load(Ordering::Relaxed);
    if done != 0 {
        return Ok(done - 1);
    }
    let off = std::env::var("NOITA_ONLINE_PATCH_OFF").unwrap_or_default();
    let skip = |name: &str| off.split(',').any(|s| s.trim() == name);
    let mut ex = Exec::new()?;
    let mut count = 0u32;
    for (name, patch) in PATCHES {
        if skip(name) {
            continue;
        }
        patch(&mut ex).map_err(|e| PatchError(format!("{name}: {}", e.0)))?;
        count += 1;
    }
    if std::env::var_os("NOITA_ONLINE_PTRACE").is_some() {
        install_particle_trace(&mut ex)?;
    }
    if std::env::var_os("NOITA_ONLINE_PROF").is_some() {
        install_job_profiler(&mut ex)?;
    }
    crate::explosions::install_log()?;
    install_heap_pad()?;
    APPLIED.store(count + 1, Ordering::Relaxed);
    Ok(count)
}

/// msvcr120 imports `malloc` and `operator new` (`??2@YAPAXI@Z`; it allocates inside
/// msvcr120, not through the `malloc` import).
const IAT_MALLOC: u32 = 0x00f0_74f0;
const IAT_NEW: u32 = 0x00f0_7500;
static ORIG_MALLOC: AtomicU32 = AtomicU32::new(0);
static ORIG_NEW: AtomicU32 = AtomicU32::new(0);
static HEAP_PAD_STATE: AtomicU32 = AtomicU32::new(0x9e37_79b9);

/// 0-112 bytes (a pseudo-random multiple of 16) to add to an allocation.
fn heap_pad() -> usize {
    let next = |mut y: u32| {
        y ^= y << 13;
        y ^= y >> 17;
        y ^ (y << 5)
    };
    let mut x = HEAP_PAD_STATE.load(Ordering::Relaxed);
    loop {
        match HEAP_PAD_STATE.compare_exchange_weak(x, next(x), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ((next(x) >> 8) & 7) as usize * 16,
            Err(v) => x = v,
        }
    }
}

/// [`install_heap_pad`]'s `malloc` and `operator new`: more than asked for, so blocks land
/// elsewhere relative to each other than on the other clients.
extern "C" fn padded_malloc(size: usize) -> *mut u8 {
    let f: extern "C" fn(usize) -> *mut u8 = unsafe { std::mem::transmute(ORIG_MALLOC.load(Ordering::Relaxed) as usize) };
    f(size.saturating_add(heap_pad()))
}
extern "C" fn padded_new(size: usize) -> *mut u8 {
    let f: extern "C" fn(usize) -> *mut u8 = unsafe { std::mem::transmute(ORIG_NEW.load(Ordering::Relaxed) as usize) };
    f(size.saturating_add(heap_pad()))
}

/// Diagnostics: `NOITA_ONLINE_HEAP_PAD=<instance>,...`: on those instances every `malloc`
/// and `operator new` after the patches are installed allocates a pseudo-random amount more
/// ([`padded_malloc`]). The heap layout, and so the relative order of heap addresses, then
/// differs from the other clients' far more often than through their own allocation
/// histories, so simulation code that orders anything by pointer (a `std::map` keyed by
/// pointer, a sort by address) diverges on most runs instead of now and then.
unsafe fn install_heap_pad() -> Result<(), PatchError> {
    let (Ok(v), Ok(inst)) = (std::env::var("NOITA_ONLINE_HEAP_PAD"), std::env::var("NOITA_ONLINE_INSTANCE")) else {
        return Ok(());
    };
    if !v.split(',').any(|s| s.trim() == inst) {
        return Ok(());
    }
    ORIG_MALLOC.store(rd(IAT_MALLOC), Ordering::Relaxed);
    ORIG_NEW.store(rd(IAT_NEW), Ordering::Relaxed);
    write_code(IAT_MALLOC, &((padded_malloc as extern "C" fn(usize) -> *mut u8) as u32).to_le_bytes())?;
    write_code(IAT_NEW, &((padded_new as extern "C" fn(usize) -> *mut u8) as u32).to_le_bytes())
}

/// Job-pool submit routines profiled by [`install_job_profiler`]: (entry, 6-byte prologue,
/// stack argument count). Jobs that run inline are timed inside the submit call.
const PROFILED_SUBMITS: [(u32, [u8; 6], u8); 2] = [
    (POOL_GROUP_FN, [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14], 2),
    (POOL_ASYNC_FN, [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x18], 1),
];

/// Diagnostics: `NOITA_ONLINE_PROF=1` times every call of the (original) job submit routines
/// by call site (inline jobs run inside them) and writes per-site totals every 600 frames to
/// `$NOITA_ONLINE_OUT/prof_<instance>.txt`. Threaded copies taken earlier are not affected.
unsafe fn install_job_profiler(ex: &mut Exec) -> Result<(), PatchError> {
    let enter = ((prof_enter as extern "C" fn(u32)) as u32).to_le_bytes();
    let exit = ((prof_exit as extern "C" fn()) as u32).to_le_bytes();
    for (f, prologue, nargs) in PROFILED_SUBMITS {
        expect(f, &prologue)?;
        // trampoline: original prologue; jmp f+6
        let tramp_at = ex.base + ex.used;
        let mut tramp = prologue.to_vec();
        tramp.push(0xE9);
        tramp.extend_from_slice(&rel32(tramp_at + 11, f + 6));
        let tramp = ex.put(&tramp);
        // push ecx; push [esp+4]; mov eax,enter; call eax; add esp,4; pop ecx;
        // n × push [esp+4n]; call tramp; push eax; mov eax,exit; call eax; pop eax; ret 4n
        let stub_at = ex.base + ex.used;
        let mut s = vec![0x51, 0xFF, 0x74, 0x24, 0x04, 0xB8, enter[0], enter[1], enter[2], enter[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x59];
        for _ in 0..nargs {
            s.extend_from_slice(&[0xFF, 0x74, 0x24, 4 * nargs]);
        }
        s.push(0xE8);
        s.extend_from_slice(&rel32(stub_at + s.len() as u32 + 4, tramp));
        s.extend_from_slice(&[0x50, 0xB8, exit[0], exit[1], exit[2], exit[3], 0xFF, 0xD0, 0x58, 0xC2, 4 * nargs, 0x00]);
        let at = ex.put(&s);
        let mut jmp = vec![0xE9];
        jmp.extend_from_slice(&rel32(f + 5, at));
        jmp.push(0x90);
        write_code(f, &jmp)?;
    }
    Ok(())
}

thread_local! {
    /// (call site, start, time spent in nested profiled calls)
    static PROF_STACK: std::cell::RefCell<Vec<(u32, std::time::Instant, std::time::Duration)>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// call site -> (calls, inclusive, exclusive)
static PROF_SITES: std::sync::Mutex<Option<std::collections::BTreeMap<u32, (u64, std::time::Duration, std::time::Duration)>>> =
    std::sync::Mutex::new(None);

extern "C" fn prof_enter(ret: u32) {
    PROF_STACK.with(|s| s.borrow_mut().push((ret, std::time::Instant::now(), std::time::Duration::ZERO)));
}

extern "C" fn prof_exit() {
    let Some((ret, t0, child)) = PROF_STACK.with(|s| s.borrow_mut().pop()) else { return };
    let dt = t0.elapsed();
    PROF_STACK.with(|s| {
        if let Some(parent) = s.borrow_mut().last_mut() {
            parent.2 += dt;
        }
    });
    let mut m = PROF_SITES.lock().unwrap_or_else(|p| p.into_inner());
    let e = m.get_or_insert_with(Default::default).entry(ret).or_default();
    e.0 += 1;
    e.1 += dt;
    e.2 += dt.saturating_sub(child);
}

/// Window timing for the profiler: (frames, time inside the simulation window).
static PROF_WINDOW: std::sync::Mutex<(u32, std::time::Duration, Option<std::time::Instant>, Option<std::time::Instant>)> =
    std::sync::Mutex::new((0, std::time::Duration::ZERO, None, None));

unsafe fn prof_window(enter: bool) {
    use std::io::Write;
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NOITA_ONLINE_PROF").is_some());
    if !*ON {
        return;
    }
    let mut w = PROF_WINDOW.lock().unwrap_or_else(|p| p.into_inner());
    if enter {
        w.2 = Some(std::time::Instant::now());
        if w.3.is_none() {
            w.3 = w.2;
        }
        return;
    }
    if let Some(t) = w.2.take() {
        w.0 += 1;
        w.1 += t.elapsed();
    }
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    if frame % 600 != 0 || w.0 == 0 {
        return;
    }
    let wall = w.3.map_or(std::time::Duration::ZERO, |t| t.elapsed());
    let (n, inside) = (w.0, w.1);
    *w = (0, std::time::Duration::ZERO, None, None);
    drop(w);
    let sites = PROF_SITES.lock().unwrap_or_else(|p| p.into_inner()).take().unwrap_or_default();
    let (Ok(out), inst) = (std::env::var("NOITA_ONLINE_OUT"), std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default()) else { return };
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::path::Path::new(&out).join(format!("prof_{inst}.txt"))) else { return };
    let per = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n as f64;
    let _ = writeln!(f, "frame {frame}: {n} frames, wall {:.2} ms/frame, sim window {:.2} ms/frame", per(wall), per(inside));
    let mut v: Vec<_> = sites.into_iter().collect();
    v.sort_by(|a, b| b.1 .2.cmp(&a.1 .2));
    for (site, (calls, incl, excl)) in v {
        let _ = writeln!(f, "  site {site:08x}: {:.1} calls/frame incl {:.3} excl {:.3} ms/frame", calls as f64 / n as f64, per(incl), per(excl));
    }
}

/// Particle slot allocators (every grid particle is created through one of them); their
/// first 10 bytes `push ebp; mov ebp,esp; cmp byte [0x01204ba4],0` are relocatable.
const PARTICLE_SLOT_FNS: [u32; 2] = [0x0071_7400, 0x0071_74e0];
const PARTICLE_SLOT_PROLOGUE: [u8; 10] = [0x55, 0x8B, 0xEC, 0x80, 0x3D, 0xA4, 0x4B, 0x20, 0x01, 0x00];

/// Diagnostics: `NOITA_ONLINE_PTRACE=from-to` logs every grid-particle creation in that frame
/// range (thread, allocator, EBP-chain return addresses, list sizes), tile dirty marks outside
/// the window, and (with `cell_flash` off) the render job's cell-flash clears to
/// `$NOITA_ONLINE_OUT/ptrace_<instance>.txt`.
unsafe fn install_particle_trace(ex: &mut Exec) -> Result<(), PatchError> {
    let cb = ((particle_trace as extern "C" fn(u32, *const u32)) as u32).to_le_bytes();
    for (tag, f) in PARTICLE_SLOT_FNS.into_iter().enumerate() {
        expect(f, &PARTICLE_SLOT_PROLOGUE)?;
        let stub_at = ex.base + ex.used;
        // pushad; push esp; push tag; mov eax,cb; call eax; add esp,8; popad; <prologue>; jmp f+10
        let mut stub = vec![0x60, 0x54, 0x6A, tag as u8, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x61];
        stub.extend_from_slice(&PARTICLE_SLOT_PROLOGUE);
        stub.push(0xE9);
        stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, f + 10));
        let at = ex.put(&stub);
        let mut jmp = vec![0xE9];
        jmp.extend_from_slice(&rel32(f + 5, at));
        write_code(f, &jmp)?;
    }
    // Tile dirty-AABB mark (GridWorldThreaded vtbl+0x14, `push ebp; mov ebp,esp; mov edx,[ebp+8]`):
    // logged only outside the simulation window, where any world write is a per-client leak.
    expect(TILE_MARK_DIRTY_FN, &TILE_MARK_DIRTY_PROLOGUE)?;
    let stub_at = ex.base + ex.used;
    let mut stub = vec![0x60, 0x54, 0x6A, 0x02, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x61];
    stub.extend_from_slice(&TILE_MARK_DIRTY_PROLOGUE);
    stub.push(0xE9);
    stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, TILE_MARK_DIRTY_FN + 6));
    let at = ex.put(&stub);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(TILE_MARK_DIRTY_FN + 5, at));
    jmp.push(0x90);
    write_code(TILE_MARK_DIRTY_FN, &jmp)?;
    // With `cell_flash` off: the render job's flash test, logged where vanilla clears the
    // flag (tag 3: cell flash older than FLASH_FRAMES, i.e. cleared right after this).
    let have = std::slice::from_raw_parts(RENDER_FLASH_TEST as *const u8, RENDER_FLASH_TEST_ORIG.len());
    if have == RENDER_FLASH_TEST_ORIG {
        let stub_at = ex.base + ex.used;
        // pushad; push esp; push 3; mov eax,cb; call eax; add esp,8; popad;
        // cmp byte [edi+0xc],0; jz RENDER_NO_FLASH; jmp RENDER_FLASH_TEST+10
        let mut stub = vec![0x60, 0x54, 0x6A, 0x03, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x61];
        stub.extend_from_slice(&[0x80, 0x7F, 0x0C, 0x00, 0x0F, 0x84]);
        stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, RENDER_NO_FLASH));
        stub.push(0xE9);
        stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, RENDER_FLASH_TEST + 10));
        let at = ex.put(&stub);
        let mut jmp = vec![0xE9];
        jmp.extend_from_slice(&rel32(RENDER_FLASH_TEST + 5, at));
        jmp.extend_from_slice(&[0x90; 5]);
        write_code(RENDER_FLASH_TEST, &jmp)?;
    }
    // Body -> grid placement (FUN_007440a0, thiscall ecx = grid body; tag 4) and particle
    // construction (FUN_00716ce0, args: material, &pos, &vel; tag 5): both `push ebp;
    // mov ebp,esp; and esp,-8`.
    for (tag, f) in [(4u8, BODY_TO_GRID_FN), (5, PARTICLE_CREATE_FN)] {
        expect(f, &BODY_PARTICLE_PROLOGUE)?;
        let stub_at = ex.base + ex.used;
        let mut stub = vec![0x60, 0x54, 0x6A, tag, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x61];
        stub.extend_from_slice(&BODY_PARTICLE_PROLOGUE);
        stub.push(0xE9);
        stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, f + 6));
        let at = ex.put(&stub);
        let mut jmp = vec![0xE9];
        jmp.extend_from_slice(&rel32(f + 5, at));
        jmp.push(0x90);
        write_code(f, &jmp)?;
    }
    // Body shapes (tags 6-9): the cell-bitmap polygonizer FUN_0095bcf0 (ecx = bitmap
    // {w, h, .., +0x18 Cell*[w*h]}, edx = shape vector, xmm2 = tolerance),
    // b2Body::CreateFixture FUN_009bc760 (ecx = body, [esp+4] = fixture def, def+0 = shape),
    // the ragdoll loader FUN_00be8240 ([esp+0x24] = stains overlay image, xmm3 = mirror) and
    // the image -> shapes builder FUN_0095b610 (ecx = image {w, h, .., +0x18 u32[w*h]}).
    // XMM0-3 are saved around the callback (xmm2/xmm3 are arguments).
    for (tag, f, prologue) in [
        (6u8, POLYGONIZE_FN, &POLYGONIZE_PROLOGUE[..]),
        (7, CREATE_FIXTURE_FN, &CREATE_FIXTURE_PROLOGUE[..]),
        (8, RAGDOLL_LOAD_FN, &RAGDOLL_LOAD_PROLOGUE[..]),
        (9, IMAGE_SHAPES_FN, &POLYGONIZE_PROLOGUE[..]),
    ] {
        expect(f, prologue)?;
        let stub_at = ex.base + ex.used;
        // sub esp,64; movups [esp+0/16/32/48],xmm0..3
        let mut stub = vec![0x83, 0xEC, 0x40, 0x0F, 0x11, 0x04, 0x24, 0x0F, 0x11, 0x4C, 0x24, 0x10, 0x0F, 0x11, 0x54, 0x24, 0x20, 0x0F, 0x11, 0x5C, 0x24, 0x30];
        stub.extend_from_slice(&[0x60, 0x54, 0x6A, tag, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x61]);
        // movups xmm0..3,[esp+..]; add esp,64
        stub.extend_from_slice(&[0x0F, 0x10, 0x04, 0x24, 0x0F, 0x10, 0x4C, 0x24, 0x10, 0x0F, 0x10, 0x54, 0x24, 0x20, 0x0F, 0x10, 0x5C, 0x24, 0x30, 0x83, 0xC4, 0x40]);
        stub.extend_from_slice(prologue);
        stub.push(0xE9);
        stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, f + prologue.len() as u32));
        let at = ex.put(&stub);
        let mut jmp = vec![0xE9];
        jmp.extend_from_slice(&rel32(f + 5, at));
        jmp.extend(std::iter::repeat(0x90).take(prologue.len() - 5));
        write_code(f, &jmp)?;
    }
    Ok(())
}

const POLYGONIZE_FN: u32 = 0x0095_bcf0;
/// push ebp; mov ebp,esp; push -1
const POLYGONIZE_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
const CREATE_FIXTURE_FN: u32 = 0x009b_c760;
/// push ebp; mov ebp,esp; sub esp,0x10
const CREATE_FIXTURE_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x10];
/// Ragdoll loader ("Ragdolls - Cached load"): fastcall, 9 stack arguments, the last one the
/// stains overlay image (0 = none). `push ebp; mov ebp,esp; and esp,-16`.
const RAGDOLL_LOAD_FN: u32 = 0x00be_8240;
const RAGDOLL_LOAD_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0];
/// Image -> Box2D shapes (FUN_009814f0 outline + decomposition, FUN_009817d0 box fallback).
const IMAGE_SHAPES_FN: u32 = 0x0095_b610;

const BODY_TO_GRID_FN: u32 = 0x0074_40a0;
const PARTICLE_CREATE_FN: u32 = 0x0071_6ce0;
const BODY_PARTICLE_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF8];

const TILE_MARK_DIRTY_FN: u32 = 0x0072_b080;
const TILE_MARK_DIRTY_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x8B, 0x55, 0x08];

/// The `NOITA_ONLINE_PTRACE=from-to` log: (from, to, `ptrace_<instance>.txt`).
static PTRACE_LOG: std::sync::LazyLock<Option<(u32, u32, std::sync::Mutex<std::fs::File>)>> = std::sync::LazyLock::new(|| {
    let v = std::env::var("NOITA_ONLINE_PTRACE").ok()?;
    let (a, b) = v.split_once('-')?;
    let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let f = std::fs::File::create(std::path::Path::new(&out).join(format!("ptrace_{inst}.txt"))).ok()?;
    Some((a.parse().ok()?, b.parse().ok()?, std::sync::Mutex::new(f)))
});

/// The current engine frame if it is inside the `NOITA_ONLINE_PTRACE` range.
unsafe fn ptrace_frame() -> Option<u32> {
    let (a, b, _) = PTRACE_LOG.as_ref()?;
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    (*a..=*b).contains(&frame).then_some(frame)
}

fn ptrace_write(line: &str) {
    use std::io::Write;
    if let Some((_, _, file)) = PTRACE_LOG.as_ref() {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Image struct {w, h, .., +0x18 u32 pixels[w*h]}: (w, h, FNV-1a over the pixels).
unsafe fn image_hash(img: u32) -> (i32, i32, u32) {
    let (w, h, data) = (rd(img) as i32, rd(img + 4) as i32, rd(img + 0x18));
    let mut hash = 0x811c_9dc5u32;
    if data != 0 && (0..1 << 20).contains(&(w * h)) {
        for i in 0..(w * h) as u32 {
            hash = (hash ^ rd(data + 4 * i)).wrapping_mul(0x0100_0193);
        }
    }
    (w, h, hash)
}

extern "C" fn particle_trace(tag: u32, regs: *const u32) {
    unsafe {
        let Some(frame) = ptrace_frame() else { return };
        if tag == 2 && IN_WINDOW.load(Ordering::Relaxed) && std::env::var_os("NOITA_ONLINE_PTRACE_ALL").is_none() {
            return;
        }
        // pushad block: edi esi ebp esp ebx edx ecx eax
        // Tags 6/7 saved 64 bytes of XMM state below the return address.
        let esp = *regs.add(3) + if tag >= 6 { 64 } else { 0 };
        let mut ebp = *regs.add(2);
        let gp = *regs.add(6);
        let what = if tag == 8 {
            // Stains overlay (SpriteStains+0x20 in vanilla, the blur scratch with
            // `ragdoll_stains`): the stain colours and the render's blurred copy
            let p = rd(esp + 0x24);
            let base = if p == 0 { 0 } else { crate::ragdoll::stains_of(p).unwrap_or(p - 0x20) };
            let ((w, h, a), (_, _, b)) = if base != 0 { (image_hash(base), image_hash(base + 0x20)) } else { ((0, 0, 0), (0, 0, 0)) };
            format!("ragdoll stains={w}x{h} colours={a:08x} blurred={b:08x} overlay={:08x}", if p == 0 { 0 } else { image_hash(p).2 })
        } else if tag == 9 {
            let (w, h, hash) = image_hash(gp);
            let mut s = format!("image_shapes w={w} h={h} hash={hash:08x} px=");
            let data = rd(gp + 0x18);
            if data != 0 && (0..=256).contains(&(w * h)) {
                for i in 0..(w * h) as u32 {
                    s.push_str(&format!("{:08x},", rd(data + 4 * i)));
                }
            }
            s
        } else if tag == 6 {
            // ecx = bitmap {w, h, .., +0x18 Cell*[w*h]}: its set pixels
            let (w, h, data) = (rd(gp) as i32, rd(gp + 4) as i32, rd(gp + 0x18));
            let mut s = format!("polygonize w={w} h={h} px=");
            if data != 0 && (0..4096).contains(&(w * h)) {
                for i in 0..(w * h) as u32 {
                    if rd(data + 4 * i) != 0 {
                        s.push_str(&format!("{},{};", i as i32 % w, i as i32 / w));
                    }
                }
            }
            s
        } else if tag == 7 {
            // b2Body::CreateFixture(def): the body's fixture count and the shape's words
            let def = rd(esp + 4);
            let shape = rd(def);
            let mut s = format!("fixture body_fx={} def=", rd(gp + 0xc0));
            for k in 1..12 {
                s.push_str(&format!("{:08x}", rd(def + 4 * k)));
            }
            s.push_str(" shape=");
            for k in 1..0x50 {
                s.push_str(&format!("{:08x}", rd(shape + 4 * k)));
            }
            s
        } else if tag == 3 {
            // edi = cell; only flags the render is about to clear
            let cell = *regs;
            if *((cell + 0xc) as *const u8) == 0 || (frame as i32).wrapping_sub(rd(cell + 8) as i32) < FLASH_FRAMES as i32 {
                return;
            }
            format!("flash_clear x={} y={} set={} vt={:08x}", rd(cell + 0x18) as i32, rd(cell + 0x1c) as i32, rd(cell + 8) as i32, rd(cell))
        } else if tag == 2 {
            format!("dirty x={} y={}", rd(esp + 4) as i32, rd(esp + 8) as i32)
        } else if tag == 4 {
            let body = gp;
            let b2 = rd(body);
            let f64at = |a: u32| *(a as *const f64);
            format!(
                "body key={} f={} cells={} chg={} pos={:?},{:?} a={:?} v={:?},{:?} w={:?} cache={:?},{:?},{:?}",
                rd(body + 0x1c) as i32,
                f32::from_bits(rd(body + 0x20)),
                (rd(body + 8).wrapping_sub(rd(body + 4))) / 4,
                *((body + 0x24) as *const u8),
                f64at(b2 + 0x18),
                f64at(b2 + 0x20),
                f64at(b2 + 0x70),
                f64at(b2 + 0x80),
                f64at(b2 + 0x88),
                f64at(b2 + 0x90),
                f64at(body + 0x50),
                f64at(body + 0x58),
                f64at(body + 0x60)
            ) + &{
                // b2Body fixture count / mass, and the cells' last grid positions (+0x28/+0x2c)
                let mut s = format!(" fx={} m={:?} at=", rd(b2 + 0xc0), f64at(b2 + 0xd0));
                let (c0, c1) = (rd(body + 4), rd(body + 8));
                for c in (c0..c1).step_by(4).take(64) {
                    let cell = rd(c);
                    if cell != 0 {
                        s.push_str(&format!("{},{};", rd(cell + 0x28) as i32, rd(cell + 0x2c) as i32));
                    }
                }
                s
            }
        } else if tag == 5 {
            let (p, v) = (rd(esp + 8), rd(esp + 12));
            format!(
                "create mat={:08x} pos={:?},{:?} vel={:?},{:?}",
                rd(esp + 4),
                f32::from_bits(rd(p)),
                f32::from_bits(rd(p + 4)),
                f32::from_bits(rd(v)),
                f32::from_bits(rd(v + 4))
            )
        } else {
            format!("n={}/{}", (rd(gp + 0x38).wrapping_sub(rd(gp + 0x34))) / 0x70, (rd(gp + 0x50).wrapping_sub(rd(gp + 0x4c))) / 0x70)
        };
        // Floating-point control state of this thread (x87 control word, SSE MXCSR).
        let mut cw: u16 = 0;
        let mut csr: u32 = 0;
        std::arch::asm!("fnstcw word ptr [{0}]", "stmxcsr dword ptr [{1}]", in(reg) &mut cw, in(reg) &mut csr);
        let mut line = format!("{frame} t{} a{tag} {what} fpu={cw:04x}/{csr:04x} ret={:08x}", GetCurrentThreadId(), rd(esp));
        for _ in 0..8 {
            if ebp <= esp || ebp > esp + 0x10_0000 || ebp & 3 != 0 {
                break;
            }
            line.push_str(&format!(" {:08x}", rd(ebp + 4)));
            ebp = rd(ebp);
        }
        ptrace_write(&line);
    }
}

type PatchFn = unsafe fn(&mut Exec) -> Result<(), PatchError>;

/// Every patch, in installation order (the threaded-job copies must be taken before the
/// inline patches modify the original submit routines).
const PATCHES: &[(&str, PatchFn)] = &[
    ("rand", patch_rand),
    ("streamclock", patch_stream_clock),
    ("chunkwait", patch_chunk_wait),
    ("threaded_chunkgen", patch_threaded_chunkgen),
    ("chunk_rng", patch_chunk_rng),
    ("chunk_frame", patch_chunk_frame),
    ("threaded_grid", patch_threaded_grid),
    ("tile_explosions", |ex| unsafe { crate::explosions::patch_tile_explosions(ex) }),
    ("threaded_render", patch_threaded_render),
    ("threaded_pathfinding", patch_threaded_pathfinding),
    ("inline_group", |_| unsafe { patch_inline_group() }),
    ("inline_async", |_| unsafe { patch_inline_async() }),
    ("particle_init", |_| unsafe { patch_particle_init() }),
    ("camera_loads", |ex| unsafe { patch_camera_loads(ex) }),
    ("fixed_dt", patch_fixed_dt),
    ("focus_freeze", patch_focus_freeze),
    ("particle_views", |ex| unsafe { crate::cosmetic::patch_particle_views(ex) }),
    ("camera_bound", |ex| unsafe { crate::cambound::patch_camera_bound(ex) }),
    ("clock", |_| unsafe { crate::clock::install(write_code) }),
    ("early_window", patch_early_window),
    ("early_input", |_| unsafe { crate::earlyinput::patch() }),
    ("freecam_static", |_| unsafe { patch_freecam_static() }),
    ("cell_flash", patch_cell_flash),
    ("cell_emitters", |ex| unsafe { crate::cosmetic::patch_cell_emitters(ex) }),
    ("rain_views", |ex| unsafe { crate::cosmetic::patch_rain(ex) }),
    ("box2d_freeze", patch_box2d_freeze),
    ("box2d_overlap", |ex| unsafe { crate::box2d::patch(ex) }),
    ("stuck_wake_aabb", patch_stuck_wake_aabb),
    ("free_solids_order", patch_free_solids_order),
    ("pathfinding_refresh", patch_pathfinding_refresh),
    ("ragdoll_stains", |ex| unsafe { crate::ragdoll::patch_ragdoll_stains_blur(ex) }),
    ("platform_frame", |_| unsafe { patch_platform_frame() }),
    ("inventory_gui", |ex| unsafe { crate::invgui::install(ex) }),
    ("analog_fly_rng", |_| unsafe { patch_analog_fly_rng() }),
    ("grid_cap", |ex| unsafe { crate::gridcap::patch_grid_cap(ex) }),
    ("sprite_events", |ex| unsafe { crate::camreaders::patch_sprite_events(ex) }),
    ("ai_camera", |ex| unsafe { crate::camreaders::patch_animal_ai(ex) }),
    ("pf_jump", |ex| unsafe { crate::camreaders::patch_pathfinding_jump(ex) }),
    ("char_effects", |ex| unsafe { crate::camreaders::patch_character_effects(ex) }),
    ("worm_bursts", |ex| unsafe { crate::camreaders::patch_worm_bursts(ex) }),
    ("camera_edge", |ex| unsafe { crate::camreaders::patch_camera_edge(ex) }),
    ("verlet_cull", |ex| unsafe { crate::camreaders::patch_verlet_cull(ex) }),
    ("emitter_views", |ex| unsafe { crate::camreaders::patch_emitter_views(ex) }),
    ("pixel_scenes", |ex| unsafe { crate::camreaders::patch_pixel_scenes(ex) }),
    ("telepathy", |ex| unsafe { crate::camreaders::patch_telepathy(ex) }),
    ("screen_shake", |ex| unsafe { crate::camreaders::patch_screen_shake(ex) }),
    ("fog_of_war", |ex| unsafe { crate::camreaders::patch_fog_of_war(ex) }),
    ("lua_camera_bounds", |ex| unsafe { crate::camreaders::patch_lua_camera_bounds(ex) }),
    ("polymorph_slots", |ex| unsafe { crate::slots::patch_polymorph(ex) }),
    ("sound_rng", |ex| unsafe { crate::slots::patch_sound_rng(ex) }),
    ("local_presentation", |ex| unsafe { crate::slots::patch_presentation(ex) }),
    ("game_over_respawn", |ex| unsafe { crate::respawn::patch_game_over_button(ex) }),
    ("death_cause", |ex| unsafe { crate::respawn::patch_death_cause(ex) }),
    ("world_teardown", |ex| unsafe { crate::nextrun::patch(ex) }),
];

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
    *(PARTICLE_RNG as *mut f64) = seed as f64;
    SESSION_SEED.store(seed, Ordering::Relaxed);
    // A next run's world reuses the old GridWorld ([`reset_run`]): its RNG gets the value
    // construction would give it (`FUN_00717c50`: the platform RNG ^ 0xc).
    if GRID_RNG_RESEED.swap(false, Ordering::Relaxed) {
        let gw = grid_world_ptr();
        if gw != 0 {
            *((gw + GW_RNG) as *mut u32) = *((PLATFORM + 0xa0) as *const u32) ^ 0xc;
        }
    }
    let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
    if rs != 0 {
        *(rs as *mut u32) = seed.wrapping_mul(0x9e37_79b9) | 1;
    }
    skew_platform_frame();
    sync_platform_frame();
    // The simulation RNG bank starts from exactly these values.
    SIM_BANK.with(|b| *b.borrow_mut() = Some(RngBank::capture()));
}

/// PlatformDesktop frame counter (+0x20), returned by its vtable+0x60 getter.
const PLATFORM_FRAME: u32 = PLATFORM + 0x20;
/// The getter: `mov eax,[ecx+0x20]; ret`.
const PLATFORM_FRAME_GETTER: u32 = 0x00db_8f00;
/// Inside the window the counter is GameGlobal's frame plus this. It is above any count a
/// client reaches before its session starts (4.8 hours of menus at 60 fps), so the counter
/// only ever jumps forward.
const PLATFORM_FRAME_BASE: u32 = 1 << 20;
static PLATFORM_FRAME_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The platform frame counter counts main-loop frames from before the world exists (62 of
/// them in a `-gamemode 0` start), so it differs between clients by the time each spent
/// before its session started, e.g. in menus. The simulation reads it at ~60 sites:
/// SpriteStainsSystem updates an entity when `(frame ^ entity_id) & 15 == 0` (or a player
/// every 3rd frame) and draws the global LCG there; the PathFindingComponent update
/// FUN_00bca030 and animal AI FUN_00b750b0 schedule work by it. It is now GameGlobal's frame
/// plus [`PLATFORM_FRAME_BASE`], set by [`fix_seeds`] and at every window entry.
unsafe fn patch_platform_frame() -> Result<(), PatchError> {
    expect(PLATFORM_FRAME_GETTER, &[0x8B, 0x41, 0x20, 0xC3])?;
    PLATFORM_FRAME_ON.store(true, Ordering::Relaxed);
    Ok(())
}

unsafe fn sync_platform_frame() {
    if !PLATFORM_FRAME_ON.load(Ordering::Relaxed) {
        return;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    *(PLATFORM_FRAME as *mut u32) = PLATFORM_FRAME_BASE.wrapping_add(frame);
}

/// Diagnostics: `NOITA_ONLINE_PF_SKEW=<instance>:<n>` adds n to that instance's platform
/// frame counter once, at the first [`fix_seeds`]: a stand-in for time spent in menus
/// (overridden when `platform_frame` is on).
unsafe fn skew_platform_frame() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Ok(v) = std::env::var("NOITA_ONLINE_PF_SKEW") else { return };
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let Some(n) = v.split_once(':').filter(|(i, _)| *i == inst).and_then(|(_, n)| n.parse::<u32>().ok()) else { return };
    if !DONE.swap(true, Ordering::Relaxed) {
        *(PLATFORM_FRAME as *mut u32) = rd(PLATFORM_FRAME).wrapping_add(n);
    }
}

// --- multi-focus streaming, particles, fixed timestep, simulation RNG bank ---------------

/// GridParticles RNG (double), seeded from the platform RNG.
const PARTICLE_RNG: u32 = 0x0122_2388;
/// Particle constructor: `and byte [esi+0x6a],0x02` / `and byte [esi+0x6c],0xfc` keep
/// uninitialised heap bits (they pick the collision world); clear them fully instead.
const PARTICLE_CTOR_AND_6A: u32 = 0x0071_213e;
const PARTICLE_CTOR_AND_6C: u32 = 0x0071_2144;
/// The camera's chunk loads: FUN_00720130 (GridWorld, the camera's int rect {x, y, w, h}),
/// called by the camera's SetRect (FUN_006f2d20) whenever its int rect changes and once at
/// world creation (FUN_007184d0 @0x00718712). Unless GridWorld+0x5cc is set, it snaps the
/// rect toward zero to 64 px, grows it by 64 px on every side and, if that differs from the
/// last one (GridWorld+0x4a8..+0x4b4), requests it and a look-ahead copy moved by 4 × the
/// snapped motion (clamped to ±512 px per axis), then stores it. These are its two
/// RequestLoad calls.
const CAMERA_LOAD_CALLS: [u32; 2] = [0x0072_0210, 0x0072_0290];
/// GridWorld::RequestLoad (thiscall ECX = GridWorld, ChunkMap*, int rect[4] {x, y, w, h};
/// `ret 8`): queues every unloaded chunk the rect touches (edges inclusive).
const REQUEST_LOAD_FN: u32 = 0x0073_dab0;
/// GridWorld: the last camera load rect ({x, y, w, h}) and the camera loads' off switch.
const GW_CAMERA_LOAD_RECT: u32 = 0x4a8;
const GW_CAMERA_LOADS_OFF: u32 = 0x5cc;
/// The camera load rect GridWorld init (`FUN_007184d0`) stores before its first camera load
/// (`[0x01054340]`).
const GW_CAMERA_LOAD_RECT_INIT: [i32; 4] = [64, 64, 128, 128];
/// GridWorld: its RNG (grid::GridRandomUtility state) and frame counter (incremented at the
/// start of every grid update, read through the world's vtable+0x10).
const GW_RNG: u32 = 4;
const GW_FRAME: u32 = 0x4fc;
/// GameGlobal: world frames (counted with the frame number by GameGlobal::Update
/// `FUN_006363b0`; a new game resets only the frame number).
const GG_WORLD_FRAMES: u32 = 4;
/// Set by [`reset_run`]: the next [`fix_seeds`] seeds the reused GridWorld's RNG.
static GRID_RNG_RESEED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The GridWorld (`[[GameGlobal+0xc]+0x44]`), 0 before the world exists.
pub(crate) unsafe fn grid_world_ptr() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    if world_data != 0 { rd(world_data + 0x44) } else { 0 }
}

/// The grid update's (`FUN_00726340`) function statics that carry state from frame to frame,
/// constructed on its first call (guard `0x01225dc0` bits 1 and 2) and kept by a new game in
/// the same process, each a `std::vector` of {x, y} int pairs (begin, end, capacity):
/// - the wake list (`0x01225d4c`): the tile-sized offsets covering GetBorders' rect, rebuilt
///   and shuffled with `rand` whenever its size changes; each update wakes the tile at the
///   offset under the GridThreadImpl's cursor (+0x44, 0 at construction `FUN_00726220`) and
///   advances it, reshuffling when it wraps;
/// - the checkerboard pass order (`0x01225c3c`): the four tile parities, set to (0,0) (1,0)
///   (0,1) (1,1) when empty and shuffled with `rand` from the previous order every update.
/// The code that constructs them (with its operand addresses), checked by `world_teardown`.
const GRID_WAKE_LIST: u32 = 0x0122_5d4c;
const GRID_PASS_ORDER: u32 = 0x0122_5c3c;
const GRID_IMPL_WAKE_CURSOR: u32 = 0x44;
/// The calling thread's Park–Miller state (`FUN_0063d1c0`, no arguments, returns a double*):
/// a heap double in noita.exe's TLS block (+0x24), created by the thread's first call with
/// `[0x01053d28]` (`movsd xmm0,[0x01053d28]` @0x0063d1e8). On the game thread the simulation
/// draws it: the verlet system's broken-link particles (`FUN_00c3a610`), damage, burning cells
/// off the tile jobs (`tile_explosions` gives tiles their own stream).
const THREAD_PM_FN: u32 = 0x0063_d1c0;
const THREAD_PM_INIT: u32 = 0x0105_3d28;
const RUN_STATICS_CODE: [(u32, &[u8]); 3] = [
    // mov eax,[0x01225dc0]; test al,1; jnz; or eax,1; mov dword [0x01225d4c],0; push ..;
    // mov [0x01225dc0],eax; mov dword [0x01225d50],0
    (
        0x0072_677d,
        &[
            0xA1, 0xC0, 0x5D, 0x22, 0x01, 0xA8, 0x01, 0x75, 0x33, 0x83, 0xC8, 0x01, 0xC7, 0x05, 0x4C, 0x5D, 0x22, 0x01, 0, 0, 0, 0, 0x68,
            0x30, 0xCF, 0xEF, 0x00, 0xA3, 0xC0, 0x5D, 0x22, 0x01, 0xC7, 0x05, 0x50, 0x5D, 0x22, 0x01,
        ],
    ),
    // mov edx,[0x01225dc0]; test dl,2; jnz; or edx,2; mov dword [0x01225c3c],0; push ..;
    // mov [0x01225dc0],edx; mov dword [0x01225c40],0
    (
        0x0072_7223,
        &[
            0x8B, 0x15, 0xC0, 0x5D, 0x22, 0x01, 0xF6, 0xC2, 0x02, 0x75, 0x3A, 0x83, 0xCA, 0x02, 0xC7, 0x05, 0x3C, 0x5C, 0x22, 0x01, 0, 0, 0,
            0, 0x68, 0xE0, 0xCE, 0xEF, 0x00, 0x89, 0x15, 0xC0, 0x5D, 0x22, 0x01, 0xC7, 0x05, 0x40, 0x5C, 0x22, 0x01,
        ],
    ),
    // movsd xmm0,[0x01053d28]; movsd [eax],xmm0; mov [esi+0x24],eax
    (0x0063_d1e8, &[0xF2, 0x0F, 0x10, 0x05, 0x28, 0x3D, 0x05, 0x01, 0xF2, 0x0F, 0x11, 0x00, 0x89, 0x86, 0x24, 0, 0, 0]),
];

/// Check the code [`reset_run`] relies on for the engine's statics (`world_teardown`).
///
/// # Safety
/// Patch installation.
pub(crate) unsafe fn check_run_statics() -> Result<(), PatchError> {
    for (at, bytes) in RUN_STATICS_CODE {
        expect(at, bytes)?;
    }
    Ok(())
}

/// The streaming keep-alive list (std::vector of 0x18-byte entries: +0 expiry frame, +0x10 /
/// +0x14 pointers to the chunk's x / y; written by `FUN_0073aa70`, e.g. for the player's
/// StreamingKeepAliveComponent), which vanilla's eviction leaves alone.
const KEEP_ALIVE_BEGIN: u32 = 0x0120_7fe4;
const KEEP_ALIVE_END: u32 = 0x0120_7fe8;
/// Magic numbers STREAMING_CHUNK_TARGET (i32) and STREAMING_FREQUENCY (f64, seconds).
const STREAMING_CHUNK_TARGET: u32 = 0x0115_3294;
const STREAMING_FREQUENCY: u32 = 0x0115_2700;
/// Unload one chunk: fastcall(ecx = ChunkMap*, edx = int[3]{cx, cy, index}, stack: 1).
const UNLOAD_CHUNK_FN: u32 = 0x0074_0e70;
/// DeathMatch::Update(this, float dt): dt is wall-clock time and feeds the simulation.
const DEATHMATCH_UPDATE: u32 = 0x006b_26f0;

unsafe fn patch_particle_init() -> Result<(), PatchError> {
    expect(PARTICLE_CTOR_AND_6A, &[0x02])?;
    expect(PARTICLE_CTOR_AND_6C, &[0xFC])?;
    write_code(PARTICLE_CTOR_AND_6A, &[0x00])?;
    write_code(PARTICLE_CTOR_AND_6C, &[0x00])
}

/// ControlsSystem (FUN_00a8d7f0), gamepad branch, option `gamepad_analog_flying` (config
/// +0xe4c): the fly button is dithered from the analog trigger with one step of the global
/// LCG per frame: `imul eax,[GLOBAL_LCG],0x343fd` @0x00a8e8c1, `mov [GLOBAL_LCG],eax`
/// @0x00a8e8d0. It runs only for an enabled ControlsComponent, i.e. the local input proxy,
/// inside the window, and only on a client playing with a gamepad: that client's simulation
/// LCG ran ahead of the others'.
const ANALOG_FLY_LCG_READ: u32 = 0x00a8_e8c1;
const ANALOG_FLY_LCG_WRITE: u32 = 0x00a8_e8d0;
/// The dither's own LCG state (per client; the dithered Fly bit is captured and sent as input).
static ANALOG_FLY_LCG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x075b_cd15);

unsafe fn patch_analog_fly_rng() -> Result<(), PatchError> {
    let lcg = GLOBAL_LCG.to_le_bytes();
    expect(ANALOG_FLY_LCG_READ, &[0x69, 0x05, lcg[0], lcg[1], lcg[2], lcg[3], 0xFD, 0x43, 0x03, 0x00])?;
    expect(ANALOG_FLY_LCG_WRITE, &[0xA3, lcg[0], lcg[1], lcg[2], lcg[3]])?;
    let own = (ANALOG_FLY_LCG.as_ptr() as u32).to_le_bytes();
    write_code(ANALOG_FLY_LCG_READ + 2, &own)?;
    write_code(ANALOG_FLY_LCG_WRITE + 1, &own)
}

/// Flag byte of the camera-load stub (1: the vanilla loads go through), 0 without the patch.
static CAMERA_LOADS_FLAG: AtomicU32 = AtomicU32::new(0);

/// The camera loads follow the camera, and outside the simulation window that is each
/// client's own render camera. Before the first window (world creation: the start camera's
/// loads, identical everywhere) they stay vanilla; from the first window entry the vanilla
/// calls are skipped and [`stream_views`] makes the same loads for every player's view,
/// inside the window.
unsafe fn patch_camera_loads(ex: &mut Exec) -> Result<(), PatchError> {
    for site in CAMERA_LOAD_CALLS {
        expect(site, &[0xE8])?;
        expect(site + 1, &rel32(site + 5, REQUEST_LOAD_FN))?;
    }
    let flag = ex.alloc_data(4);
    *(flag as *mut u8) = 1;
    // cmp byte [flag],0; je skip; jmp RequestLoad; skip: ret 8
    let at = ex.base + ex.used;
    let f = flag.to_le_bytes();
    let mut stub = vec![0x80, 0x3D, f[0], f[1], f[2], f[3], 0x00, 0x74, 0x05, 0xE9];
    stub.extend_from_slice(&rel32(at + stub.len() as u32 + 4, REQUEST_LOAD_FN));
    stub.extend_from_slice(&[0xC2, 0x08, 0x00]);
    let stub_at = ex.put(&stub);
    debug_assert_eq!(stub_at, at);
    for site in CAMERA_LOAD_CALLS {
        let mut call = vec![0xE8];
        call.extend_from_slice(&rel32(site + 5, stub_at));
        write_code(site, &call)?;
    }
    CAMERA_LOADS_FLAG.store(flag, Ordering::Relaxed);
    Ok(())
}

unsafe fn patch_fixed_dt(ex: &mut Exec) -> Result<(), PatchError> {
    expect(DEATHMATCH_UPDATE, &[0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0])?;
    let stub_at = ex.base + ex.used;
    // mov dword [esp+4], 1/60f; push ebp; mov ebp,esp; and esp,-16; jmp DEATHMATCH_UPDATE+6
    let mut stub = vec![0xC7, 0x44, 0x24, 0x04, 0x89, 0x88, 0x88, 0x3C, 0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0xE9];
    stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, DEATHMATCH_UPDATE + 6));
    let at = ex.put(&stub);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(DEATHMATCH_UPDATE + 5, at));
    jmp.push(0x90);
    write_code(DEATHMATCH_UPDATE, &jmp)
}

/// Per view (slot order): the last camera load rect, as GridWorld+0x4a8 holds the camera's.
/// `start` is the rect world creation left there, read at the first window entry, which
/// every view starts from (each player's own vanilla game starts with that camera).
struct ViewLoads {
    start: Option<[i32; 4]>,
    last: Vec<[i32; 4]>,
}
static VIEW_LOADS: std::sync::Mutex<ViewLoads> = std::sync::Mutex::new(ViewLoads { start: None, last: Vec::new() });

/// First window entry (`camera_loads`): the vanilla camera loads stop, the views start from
/// the camera load rect world creation left.
unsafe fn camera_loads_window_entry() {
    let flag = CAMERA_LOADS_FLAG.load(Ordering::Relaxed);
    if flag == 0 || *(flag as *const u8) == 0 {
        return;
    }
    *(flag as *mut u8) = 0;
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world != 0 {
        let r = |i: u32| rd(grid_world + GW_CAMERA_LOAD_RECT + 4 * i) as i32;
        VIEW_LOADS.lock().unwrap_or_else(|p| p.into_inner()).start = Some([r(0), r(1), r(2), r(3)]);
    }
}

/// The vanilla camera's chunk loads (FUN_00720130, see [`CAMERA_LOAD_CALLS`]) for every
/// player's view, in slot order: view 0 is the simulation camera itself, view k the camera's
/// rect moved by its centre's offset from view 0's (as `rain_views`); without views (before
/// the players are set up) the camera alone. Each view keeps its own last rect, so the
/// look-ahead follows that player's motion. Call once per window, after the simulation
/// camera and the views are set.
///
/// # Safety
/// Game thread, inside the simulation window, world initialised.
pub unsafe fn stream_views() {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world == 0 || CAMERA_LOADS_FLAG.load(Ordering::Relaxed) == 0 || *((grid_world + GW_CAMERA_LOADS_OFF) as *const u8) != 0 {
        return;
    }
    let mut st = VIEW_LOADS.lock().unwrap_or_else(|p| p.into_inner());
    let Some(start) = st.start else { return };
    // The camera's float rect (GG+0xc: x, y, w, h).
    let cam = |i: u32| *((world_data + 4 * i) as *const f32);
    let n = crate::views::count().max(1);
    st.last.resize(n, start);
    let request: extern "thiscall" fn(u32, u32, *const i32) = std::mem::transmute(REQUEST_LOAD_FN as usize);
    for k in 0..n {
        let (dx, dy) = if k == 0 {
            (0.0, 0.0)
        } else {
            let ((x0, y0), (xk, yk)) = (crate::views::centre(0), crate::views::centre(k));
            (xk - x0, yk - y0)
        };
        // SetRect's int rect (cvttss2si), then FUN_00720130's snapping toward zero.
        let (x, y) = ((cam(0) + dx) as i32, (cam(1) + dy) as i32);
        let rect = [x / 64 * 64 - 64, y / 64 * 64 - 64, (cam(2) as i32).wrapping_add(128), (cam(3) as i32).wrapping_add(128)];
        let last = st.last[k];
        if rect == last {
            continue;
        }
        request(grid_world, grid_world + 0x500, rect.as_ptr());
        let ahead = |a: i32, b: i32| a.wrapping_sub(b).wrapping_mul(4).clamp(-512, 512);
        let next = [rect[0] + ahead(rect[0], last[0]), rect[1] + ahead(rect[1], last[1]), rect[2], rect[3]];
        request(grid_world, grid_world + 0x500, next.as_ptr());
        st.last[k] = rect;
    }
}

/// Eviction schedule: (next frame, current interval in frames, whether a chunk was evicted
/// yet); see [`evict_chunks`].
static EVICT: std::sync::Mutex<(u32, f64, bool)> = std::sync::Mutex::new((0, 0.0, false));

/// Deterministic stand-in for the engine's chunk eviction (`FUN_00743990`, which sessions
/// switch off with STREAMING_ENABLED=0 because it runs on wall-clock time around the local
/// camera). Vanilla, once every STREAMING_FREQUENCY seconds while more than
/// STREAMING_CHUNK_TARGET chunks are loaded, unloads the loaded chunk farthest from the
/// camera (squared distance in whole chunks from the camera centre's chunk, more than 0; the
/// first in row order on ties) that is not on the streaming keep-alive list: one per tick,
/// or all above three times the target; the interval halves while twice the target are
/// loaded and returns to the setting once at most the target are. Here every player keeps
/// the target: each loaded chunk belongs to the player whose focus centre (`foci`, focus
/// rects in slot order) is nearest (lowest slot on ties), and a player with more than the
/// target loses its farthest; the schedule counts frames (seconds × 60, the next check one
/// interval after the last, with the interval as this check left it). A chunk overlapping a
/// focus rect (around the player, larger than its camera load rect) stays: vanilla's
/// farthest-first choice never reaches the screen either. With one player this is vanilla's
/// rule with the player as the camera. Loaded chunks are found inside the ChunkMap's bounding
/// box (+0x10, chunks, inclusive), as vanilla scans them.
///
/// Until the first eviction the check runs every frame. Vanilla's clock is wall time: its
/// first check is on the first world frame, and world creation plus the spawn (two frames of
/// ~0.3 s generating the spawn's chunks) take about a second, so the next one comes 1–4
/// frames after the spawn's camera loads, before most of the enemies those chunks spawn over
/// the following ~40 frames are placed: the look-ahead chunk it unloads (e.g. (2,1)) keeps
/// the ones spawned into it after the unload for the whole run. Counting 60 frames from the
/// first window instead unloaded that chunk ~50 frames later with its enemies in it (the
/// unload freezes and kills them, `FUN_00a5af60`): a 1-player bot session had 10–25 fewer
/// enemies than vanilla on every seed. Returns how many chunks were unloaded.
///
/// # Safety
/// Game thread, inside the simulation window, world initialised.
pub unsafe fn evict_chunks(foci: &[[i32; 4]], frame: u32) -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world == 0 || foci.is_empty() {
        return 0;
    }
    let chunk_map = grid_world + 0x500;
    let table = rd(chunk_map + 8) as *const u32;
    if table.is_null() {
        return 0;
    }
    let target = (rd(STREAMING_CHUNK_TARGET) as i32).max(1) as usize;
    let base = (*(STREAMING_FREQUENCY as *const f64) * 60.0).max(1.0);
    let mut schedule = EVICT.lock().unwrap_or_else(|p| p.into_inner());
    if schedule.1 <= 0.0 {
        schedule.1 = base;
    }
    if frame < schedule.0 {
        return 0;
    }
    // Focus centres in chunks, as vanilla's camera centre (`>> 9`).
    let centres: Vec<(i32, i32)> = foci.iter().map(|r| ((r[0] + r[2]).div_euclid(2) >> 9, (r[1] + r[3]).div_euclid(2) >> 9)).collect();
    // Loaded chunks in row order (y, then x, ascending): (cx, cy, table index, owner, d²).
    let mut chunks: Vec<(i32, i32, u32, usize, i64)> = Vec::new();
    let mut visit = |idx: u32| {
        if *table.add(idx as usize) == 0 {
            return;
        }
        let (cx, cy) = ((idx % 512) as i32 - 256, (idx / 512) as i32 - 256);
        let mut owner = (0, i64::MAX);
        for (i, &(px, py)) in centres.iter().enumerate() {
            let d2 = ((cx - px) as i64).pow(2) + ((cy - py) as i64).pow(2);
            if d2 < owner.1 {
                owner = (i, d2);
            }
        }
        chunks.push((cx, cy, idx, owner.0, owner.1));
    };
    let [x0, y0, x1, y1] = *((chunk_map + 0x10) as *const [i32; 4]);
    if x0 <= x1 && y0 <= y1 && (x1 as i64 - x0 as i64) < 512 && (y1 as i64 - y0 as i64) < 512 {
        for y in y0..=y1 {
            for x in x0..=x1 {
                visit((((y + 256) & 511) * 512 + ((x + 256) & 511)) as u32);
            }
        }
    } else {
        for idx in 0..512 * 512u32 {
            visit(idx);
        }
    }
    chunks.sort_by_key(|c| (c.1, c.0));
    let most = (0..foci.len()).map(|p| chunks.iter().filter(|c| c.3 == p).count()).max().unwrap_or(0);
    if most <= target {
        schedule.1 = base;
        if schedule.2 {
            schedule.0 = frame + schedule.1.round().max(1.0) as u32;
        }
        return 0;
    }
    if most >= 2 * target {
        schedule.1 *= 0.5;
    }
    schedule.0 = frame + schedule.1.round().max(1.0) as u32;
    schedule.2 = true;
    drop(schedule);
    // Chunks on the keep-alive list (entries not yet expired).
    let mut kept: Vec<(i32, i32)> = Vec::new();
    let (mut e, end) = (rd(KEEP_ALIVE_BEGIN), rd(KEEP_ALIVE_END));
    while e != 0 && e < end {
        if rd(e) as i32 > rd(gg) as i32 {
            kept.push((rd(rd(e + 0x10)) as i32, rd(rd(e + 0x14)) as i32));
        }
        e += 0x18;
    }
    // FUN_00740e70 takes (ecx, edx, one stack arg) but returns with a plain `ret`: the caller
    // pops the stack argument (vanilla call sites `add esp,4`), which no Rust ABI expresses.
    let unload = |map: u32, coords: *const i32| {
        std::arch::asm!(
            "push 1",
            "call {f}",
            "add esp, 4",
            f = in(reg) UNLOAD_CHUNK_FN,
            inout("ecx") map => _,
            inout("edx") coords => _,
            clobber_abi("C"),
        );
    };
    let in_focus = |cx: i32, cy: i32| {
        let (x0, y0) = (cx * 512, cy * 512);
        foci.iter().any(|r| x0 < r[2] && x0 + 512 > r[0] && y0 < r[3] && y0 + 512 > r[1])
    };
    let mut n = 0;
    for p in 0..foci.len() {
        let count = chunks.iter().filter(|c| c.3 == p).count();
        if count <= target {
            continue;
        }
        let mut owned: Vec<&(i32, i32, u32, usize, i64)> =
            chunks.iter().filter(|c| c.3 == p && c.4 > 0 && !in_focus(c.0, c.1) && !kept.contains(&(c.0, c.1))).collect();
        let evict = if count > 3 * target { count - 3 * target } else { 1 };
        for _ in 0..evict {
            // The farthest, the first in row order on ties.
            let Some(k) = (0..owned.len()).fold(None, |best: Option<usize>, k| match best {
                Some(b) if owned[b].4 >= owned[k].4 => Some(b),
                _ => Some(k),
            }) else {
                break;
            };
            let c = owned.remove(k);
            let coords = [c.0, c.1, c.2 as i32];
            unload(chunk_map, coords.as_ptr());
            n += 1;
        }
    }
    n
}

/// Every engine RNG state, so the simulation can run on its own stream while rendering
/// and other per-client code (camera-dependent) consumes a separate one.
#[derive(Clone, Copy)]
struct RngBank {
    lcg: u32,
    plat_a0: u32,
    plat_a4: u32,
    pm: f64,
    particle: f64,
    rand: u32,
}

impl RngBank {
    unsafe fn capture() -> Self {
        let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
        Self {
            lcg: rd(GLOBAL_LCG),
            plat_a0: rd(PLATFORM + 0xa0),
            plat_a4: rd(PLATFORM + 0xa4),
            pm: *(GLOBAL_PM as *const f64),
            particle: *(PARTICLE_RNG as *const f64),
            rand: if rs != 0 { rd(rs) } else { 0 },
        }
    }
    unsafe fn restore(&self) {
        *(GLOBAL_LCG as *mut u32) = self.lcg;
        *((PLATFORM + 0xa0) as *mut u32) = self.plat_a0;
        *((PLATFORM + 0xa4) as *mut u32) = self.plat_a4;
        *(GLOBAL_PM as *mut f64) = self.pm;
        *(PARTICLE_RNG as *mut f64) = self.particle;
        let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
        if rs != 0 {
            *(rs as *mut u32) = self.rand;
        }
    }
}

thread_local! {
    static SIM_BANK: std::cell::RefCell<Option<RngBank>> = const { std::cell::RefCell::new(None) };
    static RENDER_BANK: std::cell::RefCell<Option<RngBank>> = const { std::cell::RefCell::new(None) };
}

static IN_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the simulation window is open (game thread; pool workers run inside the window
/// that submitted their job).
pub(crate) fn in_window() -> bool {
    IN_WINDOW.load(Ordering::Relaxed)
}

/// Simulation camera position at the last window exit; consumed by the early window entry
/// of the next frame ([`early_window_entry`]). Only armed when [`patch_early_window`] is on.
static ARMED_CAMERA: std::sync::Mutex<Option<(f32, f32)>> = std::sync::Mutex::new(None);
static EARLY_WINDOW_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Entering the simulation window: stash the per-client RNG states, load the sim stream,
/// pin the platform frame counter ([`patch_platform_frame`]).
/// Idempotent within a frame (the window may already have been entered early).
///
/// # Safety
/// Game thread.
pub unsafe fn sim_rng_enter() {
    if IN_WINDOW.swap(true, Ordering::Relaxed) {
        return;
    }
    // The Box2D step of the last world update (`box2d_overlap`) has normally been joined by
    // DeathMatch::Update already.
    crate::box2d::join();
    join_render_tiles();
    // The render used this client's own screen shake (`screen_shake`): back to the shared one.
    crate::camreaders::shake_render_leave();
    // ... and the shared damage flash (`local_presentation`).
    crate::slots::flash_render_leave();
    // The previous step's state report is hashed in the background; no step may run first.
    crate::report::join();
    RENDER_BANK.with(|b| *b.borrow_mut() = Some(RngBank::capture()));
    SIM_BANK.with(|b| {
        if let Some(s) = *b.borrow() {
            s.restore();
        }
    });
    sync_platform_frame();
    camera_loads_window_entry();
    rng_log('<');
    {
        let gg = rd(GAME_GLOBAL_PTR);
        let wd = if gg != 0 { rd(gg + 0x0c) } else { 0 };
        let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
        crate::wakewatch::on_window_entry(if gw != 0 { rd(gw + GW_B2WORLD) } else { 0 });
    }
    let gg = rd(GAME_GLOBAL_PTR);
    if gg != 0 {
        crate::dump_window_entry(rd(gg));
    }
    prof_window(true);
    // What vanilla's grid render queued at the end of the last frame (`cell_emitters`).
    crate::cosmetic::emit_cell_particles();
    crate::prof::step_mark("we");
    rng_watch(true);
}

/// Leaving the simulation window: save the sim stream, put the per-client states back,
/// arm the next frame's early entry with the current simulation camera, and release the
/// camera lock ([`GG_CAMERA_LOCK`]) so the render camera can be set.
///
/// # Safety
/// Game thread.
pub unsafe fn sim_rng_leave() {
    if !IN_WINDOW.swap(false, Ordering::Relaxed) {
        return;
    }
    crate::prof::step_mark("wx");
    rng_watch(false);
    rng_log('>');
    prof_window(false);
    // Box2D safety net (`box2d_freeze`): a slow step goes out with the next input; this
    // frame's freeze request is used up. With `box2d_overlap` the step may not have run yet:
    // it is checked when it finishes.
    if !crate::box2d::active() && box2d_step_slow() {
        crate::BOX2D_STEP_SLOW.store(true, Ordering::Relaxed);
    }
    set_box2d_freeze(false);
    SIM_BANK.with(|b| *b.borrow_mut() = Some(RngBank::capture()));
    RENDER_BANK.with(|b| {
        if let Some(r) = *b.borrow() {
            r.restore();
        }
    });
    let gg = rd(GAME_GLOBAL_PTR);
    if gg != 0 && EARLY_WINDOW_ON.load(Ordering::Relaxed) {
        let cam = (*((gg + GG_CAMERA_REQUEST) as *const f32), *((gg + GG_CAMERA_REQUEST + 4) as *const f32));
        *ARMED_CAMERA.lock().unwrap_or_else(|p| p.into_inner()) = Some(cam);
    }
    if gg != 0 {
        // The engine clears the lock at the end of every frame (0x006b5b81); clearing it here
        // instead lets the mod's render camera request (OnWorldPostUpdate, unforced
        // GameSetCameraPos) through. It used to be ignored in every frame in which the forced
        // simulation camera had moved the camera, so a client whose render camera is elsewhere
        // showed the simulation camera (first player) every other frame while that player
        // stood still, and every frame while it moved.
        *((gg + GG_CAMERA_LOCK) as *mut u8) = 0;
        // The render camera (set next, by the mod) shakes with this client's own shake.
        crate::camreaders::shake_render_enter();
        // The render shows this client's own player's damage flash (`local_presentation`).
        crate::slots::flash_render_enter();
    }
    // Next entry's material particle effects, found while this client draws.
    crate::cosmetic::arm_cell_emitters();
}

/// DeathMatch::Update, right before the `OnWorldPreUpdate` dispatch loop over every mod's Lua
/// state: `mov edi,[0x01207ea0]` (end of the mod-state vector).
const PRE_UPDATE_DISPATCH: u32 = 0x006b_3201;
/// GameGlobal::SetCameraPos(this = GameGlobal, const float pos[2], bool force), thiscall.
const SET_CAMERA_POS_FN: u32 = 0x0063_6580;
/// GameGlobal: last position requested through SetCameraPos (float x, y).
const GG_CAMERA_REQUEST: u32 = 0xc8;
/// GameGlobal: camera lock (byte). SetCameraPos returns at once for an unforced request
/// while it is set. A request sets it if it is forced or moves the camera more than 256 px,
/// but only when it actually changes the camera rect; the engine clears it at the end of
/// every DeathMatch::Update (FUN_006b59d0, 0x006b5b81).
const GG_CAMERA_LOCK: u32 = 0x3c;

/// Camera update (FUN_006b3ff0, inside the simulation window): in free-camera mode it moves
/// the camera itself, by keyboard and by easing toward its own target (DeathMatch+0xac/0xb0,
/// initialised from each client's render camera), through `call FUN_006b5c30` (1 stack arg).
const FREECAM_MOVE_CALLS: [u32; 2] = [0x006b_5462, 0x006b_548f];
const FREECAM_MOVE_FN: u32 = 0x006b_5c30;

/// The lockstep camera is driven only by the mod (simulation camera inside the window,
/// render camera outside). The free camera's own movement runs inside the window and
/// reads this client's keyboard (and eases toward a per-client target), so it would move
/// the simulation camera differently per client during human play; it is removed.
unsafe fn patch_freecam_static() -> Result<(), PatchError> {
    for site in FREECAM_MOVE_CALLS {
        let rel = rel32(site + 5, FREECAM_MOVE_FN);
        expect(site, &[0xE8, rel[0], rel[1], rel[2], rel[3]])?;
    }
    for site in FREECAM_MOVE_CALLS {
        // call FUN_006b5c30 (RET 4) -> add esp,4; nop; nop
        write_code(site, &[0x83, 0xC4, 0x04, 0x90, 0x90])?;
    }
    Ok(())
}

/// The vanilla `data/scripts/init.lua` has its own `OnWorldPreUpdate` (weather: it emits rain
/// particles around the camera with `GameEmitRainParticles`), and it runs before ours, i.e.
/// with this client's render camera and render RNG stream. The rain particles are real grid
/// particles that land as cells, so every seed with rain forked within ~20 frames. The
/// simulation window (sim RNG stream + the last simulation camera) therefore opens natively
/// before the dispatch to any mod.
unsafe fn patch_early_window(ex: &mut Exec) -> Result<(), PatchError> {
    expect(PRE_UPDATE_DISPATCH, &[0x8B, 0x3D, 0xA0, 0x7E, 0x20, 0x01])?;
    let cb = ((early_window_entry as extern "C" fn()) as u32).to_le_bytes();
    // pushad; mov eax,cb; call eax; popad; mov edi,[0x01207ea0]; ret
    let stub = [0x60, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x61, 0x8B, 0x3D, 0xA0, 0x7E, 0x20, 0x01, 0xC3];
    let at = ex.put(&stub);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(PRE_UPDATE_DISPATCH + 5, at));
    call.push(0x90);
    write_code(PRE_UPDATE_DISPATCH, &call)?;
    EARLY_WINDOW_ON.store(true, Ordering::Relaxed);
    Ok(())
}

extern "C" fn early_window_entry() {
    unsafe {
        // This frame's input into the proxy, before the window (render camera, own RNG).
        crate::earlyinput::update_proxy();
        // The camera this client drew the last frame with goes out with this frame's input.
        crate::views::set_local_camera(render_camera_now());
        let Some((x, y)) = ARMED_CAMERA.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
        if rd(GAME_GLOBAL_PTR) == 0 {
            return;
        }
        sim_rng_enter();
        set_camera_pos(x, y);
    }
}

/// The camera position last requested through SetCameraPos ([`GG_CAMERA_REQUEST`]): outside
/// the window, this client's render camera (without its screen shake), rounded to whole px.
///
/// # Safety
/// Game thread.
pub unsafe fn render_camera_now() -> Option<crate::views::Camera> {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return None;
    }
    crate::views::camera_px(*((gg + GG_CAMERA_REQUEST) as *const f32), *((gg + GG_CAMERA_REQUEST + 4) as *const f32))
}

/// The camera's int view rect {x, y, w, h} (GridWorld+0x498: what the grid render snapshots
/// and the view-bound particle tests use), if the world exists.
///
/// # Safety
/// Game thread.
pub unsafe fn camera_int_rect() -> Option<[i32; 4]> {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world == 0 {
        return None;
    }
    let r = |o: u32| rd(grid_world + o) as i32;
    Some([r(0x498), r(0x49c), r(0x4a0), r(0x4a4)])
}

/// GameGlobal::SetCameraPos with `force`, then the camera lock ([`GG_CAMERA_LOCK`]) off, for
/// the simulation camera inside the window. Forcing sets the lock only if the camera moved,
/// i.e. if this client's render camera (outside the window) was elsewhere, so the lock state
/// in the window differed between clients. Cleared, it is what vanilla has at this point of
/// a frame (the engine clears it at the end of the previous one), identically everywhere:
/// unforced requests inside the window (only game scripts' GameSetCameraPos, identical on
/// every client; the free camera's update makes none) apply everywhere, a forced one (a
/// teleporting player) locks it for the rest of the frame as in vanilla, and the
/// PlatformShooterPlayerSystem updates the camera target of its player (mDesiredCameraPos,
/// which the render camera follows) only while the lock is clear (FUN_00bf2040 @0x00bf4caf).
///
/// # Safety
/// Game thread, world initialised.
pub unsafe fn set_camera_pos(x: f32, y: f32) {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return;
    }
    let set: extern "thiscall" fn(u32, *const f32, i32) = std::mem::transmute(SET_CAMERA_POS_FN as usize);
    set(gg, [x, y].as_ptr(), 1);
    *((gg + GG_CAMERA_LOCK) as *mut u8) = 0;
}

/// Diagnostics (`NOITA_ONLINE_RNGWATCH`, [`crate::rngwatch`]): watch the simulation RNGs for
/// writes from other threads while the window is open.
unsafe fn rng_watch(enter: bool) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NOITA_ONLINE_RNGWATCH").is_some());
    if !*ON {
        return;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    let wd = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
    let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
    let watch = [(GLOBAL_LCG, 4), (PARTICLE_RNG, 8), (if gw != 0 { gw + 4 } else { 0 }, 4), (rs, 4)];
    crate::rngwatch::on_window(enter, if gg != 0 { rd(gg) } else { 0 }, watch);
}

/// Diagnostics: `NOITA_ONLINE_RNGLOG=from-to` appends the simulation RNG states at every
/// window entry (`<`) and exit (`>`) in that frame range to `$NOITA_ONLINE_OUT/rng_<instance>.txt`.
unsafe fn rng_log(tag: char) {
    use std::io::Write;
    static LOG: std::sync::LazyLock<Option<(u32, u32, std::sync::Mutex<std::fs::File>)>> = std::sync::LazyLock::new(|| {
        let v = std::env::var("NOITA_ONLINE_RNGLOG").ok()?;
        let (a, b) = v.split_once('-')?;
        let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
        let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
        let f = std::fs::File::create(std::path::Path::new(&out).join(format!("rng_{inst}.txt"))).ok()?;
        Some((a.parse().ok()?, b.parse().ok()?, std::sync::Mutex::new(f)))
    });
    let Some((a, b, file)) = LOG.as_ref() else { return };
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    if frame < *a || frame > *b {
        return;
    }
    let s = RngBank::capture();
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    let gw_rng = if grid_world != 0 { rd(grid_world + 4) } else { 0 };
    // Floating-point control state of the game thread (x87 control word, SSE MXCSR).
    let mut cw: u16 = 0;
    let mut csr: u32 = 0;
    std::arch::asm!("fnstcw word ptr [{0}]", "stmxcsr dword ptr [{1}]", in(reg) &mut cw, in(reg) &mut csr);
    if let Ok(mut f) = file.lock() {
        let _ = writeln!(
            f,
            "{frame} {tag} lcg={:08x} plat={:08x}/{:08x} pm={:.0} part={:.0} rand={:08x} gw={gw_rng:08x} t={:08x} pf={} fp={cw:04x}/{csr:04x}",
            s.lcg,
            s.plat_a0,
            s.plat_a4,
            s.pm,
            s.particle,
            s.rand,
            if gg != 0 { rd(gg + 8) } else { 0 },
            rd(PLATFORM + 0x20)
        );
        if world_data != 0 && grid_world != 0 {
            let fl = |o: u32| *((world_data + o) as *const f32);
            let gi = |o: u32| rd(grid_world + o) as i32;
            let gp = rd(grid_world + 0x540);
            let snap = if gp != 0 { rd(gp + 0x24) } else { 0 };
            let (sw, sh) = if snap != 0 { (rd(snap) as i32, rd(snap + 4) as i32) } else { (0, 0) };
            let b2 = rd(grid_world + GW_B2WORLD);
            let b2ms = if b2 != 0 { *((b2 + B2_STEP_MS) as *const f64) } else { 0.0 };
            let _ = writeln!(
                f,
                "C {frame} {tag} cam={},{},{},{} off={},{} req={},{} lock={} snap={sw}x{sh} view={},{},{},{} borders={},{},{},{} b2ms={b2ms:.3} pfc={},{} pfs={:x}",
                fl(0),
                fl(4),
                fl(8),
                fl(12),
                fl(0x50),
                fl(0x54),
                *((gg + GG_CAMERA_REQUEST) as *const f32),
                *((gg + GG_CAMERA_REQUEST + 4) as *const f32),
                *((gg + GG_CAMERA_LOCK) as *const u8),
                gi(0x498),
                gi(0x49c),
                gi(0x4a0),
                gi(0x4a4),
                gi(0x4a8),
                gi(0x4ac),
                gi(0x4b0),
                gi(0x4b4),
                rd(PF_REFRESH_STATE + 4) as i32,
                rd(PF_REFRESH_STATE + 8) as i32,
                rd(PF_REFRESH_STATE + 0x18)
            );
            let _ = writeln!(f, "V {frame} {tag} {}", crate::camreaders::debug_line());
            let _ = f.write_all(body_hash_lines(frame, tag, grid_world).as_bytes());
            let _ = writeln!(f, "G {frame} {tag} {}", grid_summary(grid_world));
        }
        if tag == '<' && std::env::var_os("NOITA_ONLINE_RNGLOG_TILES").is_some() {
            let _ = f.write_all(grid_tiles_dump(frame).as_bytes());
        }
    }
}

/// Diagnostics for [`rng_log`]: `B frame tag count hash` over every b2World body's motion
/// state (dwords 6..43: transform, sweep, velocities; doubles), and with
/// `NOITA_ONLINE_RNGLOG_BODIES=1` one `b frame tag index hash x y` line per body.
unsafe fn body_hash_lines(frame: u32, tag: char, grid_world: u32) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let b2 = rd(grid_world + GW_B2WORLD);
    if b2 == 0 {
        return out;
    }
    let each = std::env::var_os("NOITA_ONLINE_RNGLOG_BODIES").is_some();
    let (n, all, sleep) = body_hashes(b2, |i, body, h, flags, st| {
        if each {
            let x = *((body + 0x18) as *const f64);
            let y = *((body + 0x20) as *const f64);
            let _ = writeln!(out, "b {frame} {tag} {i} {h:08x} {x} {y} t{} f{flags:04x} s{st}", rd(body + 8));
        }
    });
    let _ = writeln!(out, "B {frame} {tag} {n} {all:08x} sleep={sleep:08x}");
    out
}

/// Diagnostics for [`rng_log`]: a cheap summary of the falling-sand state that diverges before
/// the global RNGs do: the active tile list (count, deferred count, order hash), a hash of
/// every tile's RNG (tile map in key order), the GridWorld frame counter, and per GridParticles
/// list (+0x34/+0x40/+0x4c, 0x70 bytes each) its length and a hash of the particles' positions
/// and velocities (dwords 7..10).
unsafe fn grid_summary(grid_world: u32) -> String {
    use std::fmt::Write;
    let fnv = |h: u32, v: u32| (h ^ v).wrapping_mul(0x0100_0193);
    let mut out = String::new();
    let imp = rd(grid_world + 0x5d0);
    if imp != 0 {
        let tile_id = |t: u32| ((rd(t + 0x540) & 0xffff) << 16) | (rd(t + 0x544) & 0xffff);
        let (count, deferred) = (rd(imp), rd(imp + 0x30));
        let mut order = 0x811c_9dc5u32;
        for i in 0..count.min(65536) {
            order = fnv(order, tile_id(rd(rd(imp + 4) + 4 * i)));
        }
        let (mut tiles, mut rng) = (0u32, 0x811c_9dc5u32);
        for_each_tile(imp, |tile| {
            tiles += 1;
            rng = fnv(fnv(rng, tile_id(tile)), rd(tile + 4));
        });
        let _ = write!(out, "act={count} def={deferred} order={order:08x} tiles={tiles} trng={rng:08x}");
    }
    // Chunk jobs pending in the engine's map (submitted, not yet integrated).
    let _ = write!(out, " gf={} jobs={}", rd(grid_world + 0x4fc), chunk_jobs().len());
    let gp = rd(grid_world + 0x540);
    if gp != 0 {
        for list in [0x34u32, 0x40, 0x4c] {
            let (b, e) = (rd(gp + list), rd(gp + list + 4));
            let mut h = 0x811c_9dc5u32;
            let mut n = 0u32;
            let mut p = b;
            while p < e && n < 1_000_000 {
                for k in 7..11 {
                    h = fnv(h, rd(p + 4 * k));
                }
                n += 1;
                p += 0x70;
            }
            let _ = write!(out, " p{list:x}={n}/{h:08x}");
        }
    }
    out
}

/// Diagnostics (`box2d_overlap` check mode): every RNG of the bank plus the GridWorld RNG.
pub(crate) unsafe fn rng_fingerprint() -> u64 {
    let s = RngBank::capture();
    let gg = rd(GAME_GLOBAL_PTR);
    let wd = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in [s.lcg as u64, s.plat_a0 as u64, s.plat_a4 as u64, s.pm.to_bits(), s.particle.to_bits(), s.rand as u64, if gw != 0 { rd(gw + 4) as u64 } else { 0 }] {
        h = (h ^ v).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Diagnostics (`box2d_overlap` check mode): count, motion and sleep hashes of every body.
pub(crate) unsafe fn bodies_fingerprint(grid_world: u32) -> u64 {
    let b2 = rd(grid_world + GW_B2WORLD);
    if b2 == 0 {
        return 0;
    }
    let (n, all, sleep) = body_hashes(b2, |_, _, _, _, _| {});
    (((all as u64) << 32) | sleep as u64) ^ ((n as u64) << 16)
}

/// Every b2World body in list order: (count, motion hash, sleep hash), calling `each(index,
/// body, body motion hash, flags, sleep time)`. The motion hash covers dwords 6..43
/// (transform, sweep, velocities); the sleep hash the flags (awake 2, active 0x20; the island
/// flag 1 masked) and the sleep time.
unsafe fn body_hashes(b2: u32, mut each: impl FnMut(u32, u32, u32, u16, f64)) -> (u32, u32, u32) {
    let (mut all, mut n) = (0x811c_9dc5u32, 0u32);
    let mut sleep = 0x811c_9dc5u32;
    let mut body = rd(b2 + 0x1922c);
    while body != 0 && n < 100_000 {
        let mut h = 0x811c_9dc5u32;
        for k in 6..44 {
            h = (h ^ rd(body + 4 * k)).wrapping_mul(0x0100_0193);
        }
        all = (all ^ h).wrapping_mul(0x0100_0193);
        let flags = *((body + 0xc) as *const u16) & !1;
        let st = *((body + 0x108) as *const f64);
        for v in [flags as u32, st.to_bits() as u32, (st.to_bits() >> 32) as u32] {
            sleep = (sleep ^ v).wrapping_mul(0x0100_0193);
        }
        each(n, body, h, flags, st);
        n += 1;
        body = rd(body + 0xb8);
    }
    (n, all, sleep)
}

/// Diagnostics for `NOITA_ONLINE_STEPLOG` ([`crate::prof`]): if the game frame is in
/// `from..=to`, the frame and the state a step's stages change: whether the window is open,
/// every RNG (the simulation's inside the window), the GridWorld RNG, the Box2D body count
/// and motion / sleep hashes.
pub(crate) unsafe fn step_state(from: u32, to: u32) -> Option<(u32, String)> {
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    if gg == 0 || frame < from || frame > to {
        return None;
    }
    let s = RngBank::capture();
    let wd = rd(gg + 0x0c);
    let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
    let b2 = if gw != 0 { rd(gw + GW_B2WORLD) } else { 0 };
    let (n, all, sleep) = if b2 != 0 { body_hashes(b2, |_, _, _, _, _| {}) } else { (0, 0, 0) };
    let line = format!(
        "w={} lcg={:08x} plat={:08x}/{:08x} pm={:.0} part={:.0} rand={:08x} gw={:08x} nb={n} bh={all:08x} sl={sleep:08x}",
        in_window() as u8,
        s.lcg,
        s.plat_a0,
        s.plat_a4,
        s.pm,
        s.particle,
        s.rand,
        if gw != 0 { rd(gw + 4) } else { 0 }
    );
    Some((frame, line))
}

/// Diagnostics for `NOITA_ONLINE_STEPLOG_LITE` ([`crate::prof`]): like [`step_state`], but one
/// FNV hash of the window flag, every RNG, the GridWorld RNG, the Box2D body count and the
/// three GridParticles list lengths (no body motion hash: cheap enough for a whole run).
pub(crate) unsafe fn step_hash(from: u32, to: u32) -> Option<(u32, u32)> {
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    if gg == 0 || frame < from || frame > to {
        return None;
    }
    let fnv = |h: u32, v: u32| (h ^ v).wrapping_mul(0x0100_0193);
    let s = RngBank::capture();
    let wd = rd(gg + 0x0c);
    let gw = if wd != 0 { rd(wd + 0x44) } else { 0 };
    let mut h = 0x811c_9dc5u32;
    let (pm, part) = (s.pm.to_bits(), s.particle.to_bits());
    for v in [in_window() as u32, s.lcg, s.plat_a0, s.plat_a4, pm as u32, (pm >> 32) as u32, part as u32, (part >> 32) as u32, s.rand] {
        h = fnv(h, v);
    }
    if gw != 0 {
        h = fnv(h, rd(gw + 4));
        let b2 = rd(gw + GW_B2WORLD);
        let mut n = 0u32;
        let mut body = if b2 != 0 { rd(b2 + 0x1922c) } else { 0 };
        while body != 0 && n < 100_000 {
            n += 1;
            body = rd(body + 0xb8);
        }
        h = fnv(h, n);
        let gp = rd(gw + 0x540);
        if gp != 0 {
            for list in [0x34u32, 0x40, 0x4c] {
                h = fnv(h, rd(gp + list + 4).wrapping_sub(rd(gp + list)) / 0x70);
            }
        }
    }
    Some((frame, h))
}

/// Diagnostics: every 64x64 grid tile (tile map in key order) as `T frame x y rng` and the
/// active list in order as `A frame x,y ...`.
unsafe fn grid_tiles_dump(frame: u32) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let imp = grid_impl();
    if imp == 0 {
        return out;
    }
    for_each_tile(imp, |tile| {
        let r = |o: u32| rd(tile + o) as i32;
        let _ = writeln!(
            out,
            "T {frame} {} {} {:08x} d={},{},{},{} u={},{},{},{} w={}",
            r(0x540),
            r(0x544),
            rd(tile + 4),
            r(0x480),
            r(0x484),
            r(0x488),
            r(0x48c),
            r(0x550),
            r(0x554),
            r(0x558),
            r(0x55c),
            *((tile + 0x46c) as *const u8)
        );
    });
    let count = rd(imp) as usize;
    let arr = rd(imp + 4);
    let _ = write!(out, "A {frame} {count}");
    for i in 0..count.min(4096) {
        let tile = rd(arr + 4 * i as u32);
        let _ = write!(out, " {},{}", rd(tile + 0x540) as i32, rd(tile + 0x544) as i32);
    }
    out.push('\n');
    out
}

/// The threaded grid (GridWorld+0x5d0: active list {count, tiles, .., +0x30 deferred count,
/// +0x34 deferred tiles}, +0x40 tile manager), or 0.
unsafe fn grid_impl() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world != 0 {
        rd(grid_world + 0x5d0)
    } else {
        0
    }
}

/// Every tile of the threaded grid's tile map (manager +0x14), in key order.
unsafe fn for_each_tile(imp: u32, mut f: impl FnMut(u32)) {
    let mgr = rd(imp + 0x40);
    let head = if mgr != 0 { rd(mgr + 0x14) } else { 0 };
    if head == 0 {
        return;
    }
    // in-order walk of the MSVC red-black tree (node: left, parent, right, .., isnil +0xd)
    let mut stack = Vec::new();
    let mut n = rd(head + 4);
    loop {
        while n != 0 && n != head && *((n + 0xd) as *const u8) == 0 {
            stack.push(n);
            n = rd(n);
        }
        let Some(m) = stack.pop() else { break };
        let tile = rd(m + 0x20);
        if tile != 0 {
            f(tile);
        }
        n = rd(m + 8);
    }
}

/// Diagnostics for `NOITA_ONLINE_STEPLOG_TILES` ([`crate::prof`]): the active tile list
/// (count, deferred count, hash of its order) and every tile of the tile map as
/// `x,y:rng:cells:meta` (tile index, its RNG, an FNV hash of its rect's material ids, a hash of
/// its dirty / update rects and wake flags).
pub(crate) unsafe fn step_tiles() -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let imp = grid_impl();
    let gg = rd(GAME_GLOBAL_PTR);
    let table = chunk_table_ptr();
    if imp == 0 || table == 0 {
        return out;
    }
    let fnv = |h: u32, v: u32| (h ^ v).wrapping_mul(0x0100_0193);
    let tile_id = |t: u32| ((rd(t + 0x540) & 0xffff) << 16) | (rd(t + 0x544) & 0xffff);
    let (count, deferred) = (rd(imp), rd(imp + 0x30));
    let mut order = 0x811c_9dc5u32;
    for i in 0..count.min(65536) {
        order = fnv(order, tile_id(rd(rd(imp + 4) + 4 * i)));
    }
    for i in 0..deferred.min(65536) {
        order = fnv(order, tile_id(rd(rd(imp + 0x34) + 4 * i)));
    }
    let _ = write!(out, "act={count} def={deferred} order={order:08x}");
    let materials = rd(rd(gg + 0x18) + 0x18);
    for_each_tile(imp, |tile| {
        let r = |o: u32| rd(tile + o) as i32;
        let (x0, y0, w, h) = (r(0x470), r(0x474), r(0x478), r(0x47c));
        let mut cells = 0x811c_9dc5u32;
        let chunk_of = |x: i32, y: i32| rd(table + 4 * ((((y >> 9) + 256) & 511) as u32 * 512 + (((x >> 9) + 256) & 511) as u32));
        for y in y0..y0.saturating_add(h.clamp(0, 512)) {
            let mut chunk = chunk_of(x0, y);
            for x in x0..x0.saturating_add(w.clamp(0, 512)) {
                if x & 511 == 0 {
                    chunk = chunk_of(x, y);
                }
                let cell = if chunk != 0 { rd(rd(chunk) + 4 * ((((y & 511) as u32) << 9) | (x & 511) as u32)) } else { 0 };
                let id = if cell != 0 { rd(cell + 0x14).wrapping_sub(materials) / 0x290 } else { 0xffff };
                cells = fnv(cells, id);
            }
        }
        let mut rects = 0x811c_9dc5u32;
        for o in [0x480, 0x484, 0x488, 0x48c, 0x550, 0x554, 0x558, 0x55c] {
            rects = fnv(rects, rd(tile + o));
        }
        let flags = u32::from_le_bytes([
            *((tile + 0x46c) as *const u8),
            *((tile + 0x46d) as *const u8),
            *((tile + 0x46e) as *const u8),
            *((tile + 0x53c) as *const u8),
        ]);
        let _ = write!(out, " {},{}:{:x}:{cells:x}:{rects:x}:{flags:x}:{:x}", r(0x540), r(0x544), rd(tile + 4), rd(tile + 0x538));
    });
    out
}

/// The chunk table (GridWorld+0x500 ChunkMap, +8), or 0 without a world.
unsafe fn chunk_table_ptr() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world != 0 {
        rd(grid_world + 0x508)
    } else {
        0
    }
}

/// GridWorld::OnCameraRectChanged's call that freezes/destroys physics bodies far from the
/// (render!) camera once it moved > 512 px (FUN_007212b0 -> FUN_00721050).
const CAMERA_FREEZE_CALL: u32 = 0x0072_02a7;
/// Inside FUN_00721050: 26 bytes computing dist^2(body, camera centre) into XMM2.
const FREEZE_DIST_SITE: u32 = 0x0072_1177;
const FREEZE_DIST_ORIG: [u8; 26] = [
    0x0F, 0x28, 0xD3, 0xF3, 0x0F, 0x5C, 0xD0, 0x0F, 0x28, 0xC4, 0xF3, 0x0F, 0x5C, 0xC1, 0xF3, 0x0F, 0x59, 0xD2,
    0xF3, 0x0F, 0x59, 0xC0, 0xF3, 0x0F, 0x58, 0xD0,
];
/// FUN_00721050(this = GridWorld, rect): far-body freeze pass (thiscall, RET 4).
const FREEZE_PASS_FN: u32 = 0x0072_1050;
const MAX_FOCI: usize = 16;

static FREEZE_FOCI_N: AtomicU32 = AtomicU32::new(0);
static FREEZE_FOCI_ARR: AtomicU32 = AtomicU32::new(0);

/// Body freezing follows every player's view (the view centres, `views`) instead of each
/// client's render camera: the camera-triggered pass is removed, the distance test uses the
/// nearest view centre, and the pass is triggered deterministically by [`focus_freeze_step`].
unsafe fn patch_focus_freeze(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CAMERA_FREEZE_CALL, &[0xE8, 0x04, 0x10, 0x00, 0x00])?;
    expect(FREEZE_DIST_SITE, &FREEZE_DIST_ORIG)?;
    let n = ex.alloc_data(4);
    let foci = ex.alloc_data((MAX_FOCI * 8) as u32);
    let save = ex.alloc_data(16);
    let scratch = ex.alloc_data(4);
    let (n_, f_, s_, t_) = (n.to_le_bytes(), foci.to_le_bytes(), save.to_le_bytes(), scratch.to_le_bytes());
    // In: XMM0 = body x, XMM1 = body y. Out: XMM2 = min over foci of dist^2.
    let mut stub = vec![0x50, 0x51, 0x0F, 0x11, 0x2D];
    stub.extend_from_slice(&s_); // movups [save], xmm5
    stub.extend_from_slice(&[0x8B, 0x0D]);
    stub.extend_from_slice(&n_); // mov ecx, [n]
    stub.push(0xB8);
    stub.extend_from_slice(&f_); // mov eax, foci
    stub.extend_from_slice(&[0xC7, 0x05]);
    stub.extend_from_slice(&t_);
    stub.extend_from_slice(&[0xFF, 0xFF, 0x7F, 0x7F]); // mov [scratch], FLT_MAX
    stub.extend_from_slice(&[0xF3, 0x0F, 0x10, 0x15]);
    stub.extend_from_slice(&t_); // movss xmm2, [scratch]
    stub.extend_from_slice(&[0x85, 0xC9, 0x74, 0x33]); // lp: test ecx,ecx; jz done
    stub.extend_from_slice(&[0xF3, 0x0F, 0x10, 0x28, 0xF3, 0x0F, 0x5C, 0xE8, 0xF3, 0x0F, 0x59, 0xED, 0xF3, 0x0F, 0x11, 0x2D]);
    stub.extend_from_slice(&t_);
    stub.extend_from_slice(&[0xF3, 0x0F, 0x10, 0x68, 0x04, 0xF3, 0x0F, 0x5C, 0xE9, 0xF3, 0x0F, 0x59, 0xED, 0xF3, 0x0F, 0x58, 0x2D]);
    stub.extend_from_slice(&t_);
    stub.extend_from_slice(&[0xF3, 0x0F, 0x5D, 0xD5, 0x83, 0xC0, 0x08, 0x49, 0xEB, 0xC9, 0x0F, 0x10, 0x2D]);
    stub.extend_from_slice(&s_); // done: movups xmm5, [save]
    stub.extend_from_slice(&[0x59, 0x58, 0xC3]);
    let at = ex.put(&stub);
    write_code(CAMERA_FREEZE_CALL, &[0x83, 0xC4, 0x04, 0x90, 0x90])?;
    let mut site = vec![0xE8];
    site.extend_from_slice(&rel32(FREEZE_DIST_SITE + 5, at));
    site.extend_from_slice(&[0x90; 21]);
    write_code(FREEZE_DIST_SITE, &site)?;
    FREEZE_FOCI_N.store(n, Ordering::Relaxed);
    FREEZE_FOCI_ARR.store(foci, Ordering::Relaxed);
    Ok(())
}

/// Liquid-cell update (FUN_0070c6e0, CLiquidCell vtbl+0x54): `cmp byte [edi+0xc],0; jz +0xf`
/// guarding `mark_dirty(x, y, 0)`.
const LIQUID_FLASH_TEST: u32 = 0x0070_c6ef;
/// Grid render job (FUN_00724580, outside the simulation window, render view rect):
/// `cmp byte [edi+0xc],0; jz 0x00724d23` choosing the flash-colour path.
const RENDER_FLASH_TEST: u32 = 0x0072_4b04;
const RENDER_FLASH_TEST_ORIG: [u8; 10] = [0x80, 0x7F, 0x0C, 0x00, 0x0F, 0x84, 0x15, 0x02, 0x00, 0x00];
const RENDER_NO_FLASH: u32 = 0x0072_4d23;
/// Inside the render job's flash path: `mov byte [edi+0xc],0` once the flash is 21 frames old.
const RENDER_FLASH_CLEAR: u32 = 0x0072_4bdf;
/// Frames a cell flash lasts (`cmp eax,0x15` in the render job).
const FLASH_FRAMES: u8 = 21;

/// Cell "flash" (cell+0xc flag, cell+8 = frame it was set; set by material conversion,
/// FUN_00baac80): the liquid update keeps a flashing cell's tile dirty, but the flag was only
/// ever cleared by the grid *render* job, for cells inside this client's render view, 21
/// frames after it was set. Clients looking at different places therefore kept different
/// tiles awake and forked (seed 2046634918, f=943). The expiry moves into the simulation:
/// the liquid update clears an expired flag itself (after its dirty mark, i.e. the frame
/// the on-screen render would have cleared it), and the render only reads the flag (an
/// expired flag renders as no flash, as it would after vanilla's clear).
unsafe fn patch_cell_flash(ex: &mut Exec) -> Result<(), PatchError> {
    expect(LIQUID_FLASH_TEST, &[0x80, 0x7F, 0x0C, 0x00, 0x74, 0x0F])?;
    expect(RENDER_FLASH_TEST, &RENDER_FLASH_TEST_ORIG)?;
    expect(RENDER_FLASH_CLEAR, &[0xC6, 0x47, 0x0C, 0x00])?;
    let gg = GAME_GLOBAL_PTR.to_le_bytes();
    // eax = frame - cell+8:  push eax; mov eax,[GAME_GLOBAL_PTR]; mov eax,[eax]; sub eax,[edi+8]; cmp eax,FLASH_FRAMES
    let age = [0x50, 0xA1, gg[0], gg[1], gg[2], gg[3], 0x8B, 0x00, 0x2B, 0x47, 0x08, 0x83, 0xF8, FLASH_FRAMES];
    // Liquid update: cmp byte [edi+0xc],0; jz no_flag; <age>; jl keep; mov byte [edi+0xc],0;
    // keep: pop eax; ret (-> mark dirty); no_flag: mov dword [esp],0x0070c704; ret
    let mut sim = vec![0x80, 0x7F, 0x0C, 0x00, 0x74, 0x00];
    sim.extend_from_slice(&age);
    sim.extend_from_slice(&[0x7C, 0x04, 0xC6, 0x47, 0x0C, 0x00, 0x58, 0xC3]);
    sim[5] = (sim.len() - 6) as u8;
    sim.extend_from_slice(&[0xC7, 0x04, 0x24]);
    sim.extend_from_slice(&(LIQUID_FLASH_TEST + 6 + 0x0F).to_le_bytes());
    sim.push(0xC3);
    let sim_at = ex.put(&sim);
    // Render: cmp byte [edi+0xc],0; jz off; <age>; pop eax; jge off; ret (-> flash path);
    // off: mov dword [esp],RENDER_NO_FLASH; ret
    let mut render = vec![0x80, 0x7F, 0x0C, 0x00, 0x74, 0x00];
    render.extend_from_slice(&age);
    render.extend_from_slice(&[0x58, 0x7D, 0x01, 0xC3]);
    render[5] = (render.len() - 6) as u8;
    render.extend_from_slice(&[0xC7, 0x04, 0x24]);
    render.extend_from_slice(&RENDER_NO_FLASH.to_le_bytes());
    render.push(0xC3);
    let render_at = ex.put(&render);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(LIQUID_FLASH_TEST + 5, sim_at));
    call.push(0x90);
    write_code(LIQUID_FLASH_TEST, &call)?;
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(RENDER_FLASH_TEST + 5, render_at));
    call.extend_from_slice(&[0x90; 5]);
    write_code(RENDER_FLASH_TEST, &call)?;
    // Unreachable now; the render must never write world state.
    write_code(RENDER_FLASH_CLEAR, &[0x0F, 0x1F, 0x40, 0x00])?;
    Ok(())
}

/// GridWorld: the b2World (GridWorld+0x530); b2World+0x19270: wall-clock duration of the last
/// Box2D step in ms (QueryPerformanceCounter, written at the end of b2World::Step FUN_009c0d00).
const GW_B2WORLD: u32 = 0x530;
const B2_STEP_MS: u32 = 0x19270;

/// The b2World (GridWorld+0x530), 0 without a world.
pub(crate) unsafe fn b2_world() -> u32 {
    let gw = grid_world_ptr();
    if gw != 0 {
        rd(gw + GW_B2WORLD)
    } else {
        0
    }
}
/// Body->grid update FUN_00718d50: `cmp byte [BOX2D_FREEZE_STUCK_BODIES],0; push esi;
/// push edi; mov edi,ecx; mov [esp+0x38],edi; jz 0x00718d91` (skips the freeze test).
const FREEZE_STUCK_TEST: u32 = 0x0071_8d5c;
const FREEZE_STUCK_TEST_ORIG: [u8; 17] = [0x80, 0x3D, 0xF4, 0x32, 0x15, 0x01, 0x00, 0x56, 0x57, 0x8B, 0xF9, 0x89, 0x7C, 0x24, 0x38, 0x74, 0x24];
/// The freeze test right after: `movss xmm1,[BOX2D_THREAD_MAX_WAIT_IN_MS]; mov eax,[edi+0x530];
/// cvtps2pd xmm1,xmm1; movsd xmm0,[eax+0x19270]; comisd xmm0,xmm1; jbe 0x00718d91`, then
/// `call FUN_00717aa0` (0x00718d8c) freezes the most stuck body.
const FREEZE_STUCK_TIME_TEST: u32 = 0x0071_8d6d;
const FREEZE_STUCK_TIME_TEST_ORIG: [u8; 31] = [
    0xF3, 0x0F, 0x10, 0x0D, 0x88, 0x32, 0x15, 0x01, 0x8B, 0x87, 0x30, 0x05, 0x00, 0x00, 0x0F, 0x5A, 0xC9, 0xF2, 0x0F, 0x10,
    0x80, 0x70, 0x92, 0x01, 0x00, 0x66, 0x0F, 0x2F, 0xC1, 0x76, 0x05,
];
const FREEZE_STUCK_SKIP: u32 = 0x0071_8d91;
/// Magic numbers BOX2D_FREEZE_STUCK_BODIES (bool, default 1) and BOX2D_THREAD_MAX_WAIT_IN_MS
/// (float, default 12).
const MAGIC_FREEZE_STUCK_BODIES: u32 = 0x0115_32f4;
const MAGIC_THREAD_MAX_WAIT_MS: u32 = 0x0115_3288;
/// Address of the byte the patched test reads: freeze the most stuck body in this frame's
/// body->grid update (0 = patch not installed).
static FREEZE_STUCK_NOW: AtomicU32 = AtomicU32::new(0);

/// At the start of the body->grid update (inside the window), with the magic number
/// BOX2D_FREEZE_STUCK_BODIES (on by default), the engine freezes the awake body with the
/// largest stuck counter (FUN_00717aa0) whenever the *previous Box2D step took longer than
/// BOX2D_THREAD_MAX_WAIT_IN_MS (12 ms) of wall-clock time* (b2World+0x19270): a safety net
/// for physics too heavy for the machine. Physics-heavy moments (an explosion's debris) sit
/// near that threshold, so clients froze a body at different frames (seed 1717171717,
/// f=1382, in most runs). The test now reads a flag that every client sets from the tick:
/// each client measures its own step at window exit and reports a slow step with its next
/// input ([`box2d_step_slow`]); the frame whose tick carries such a report freezes the most
/// stuck body on every client. So the safety net still fires when the slowest machine's
/// physics falls behind, one input delay later than on that machine alone.
unsafe fn patch_box2d_freeze(ex: &mut Exec) -> Result<(), PatchError> {
    expect(FREEZE_STUCK_TEST, &FREEZE_STUCK_TEST_ORIG)?;
    expect(FREEZE_STUCK_TIME_TEST, &FREEZE_STUCK_TIME_TEST_ORIG)?;
    let flag = ex.alloc_data(4);
    // cmp byte [flag],0; jz skip; nop... (falls through to the freeze call)
    let mut c = vec![0x80, 0x3D];
    c.extend_from_slice(&flag.to_le_bytes());
    c.extend_from_slice(&[0x00, 0x0F, 0x84]);
    let end = FREEZE_STUCK_TIME_TEST + c.len() as u32 + 4;
    c.extend_from_slice(&rel32(end, FREEZE_STUCK_SKIP));
    c.resize(FREEZE_STUCK_TIME_TEST_ORIG.len(), 0x90);
    write_code(FREEZE_STUCK_TIME_TEST, &c)?;
    FREEZE_STUCK_NOW.store(flag, Ordering::Relaxed);
    Ok(())
}

/// Whether the last Box2D step took longer than BOX2D_THREAD_MAX_WAIT_IN_MS (game thread, after
/// the step: window exit, or where `box2d_overlap` finished it; false without the freeze patch
/// or with BOX2D_FREEZE_STUCK_BODIES off).
pub(crate) unsafe fn box2d_step_slow() -> bool {
    if FREEZE_STUCK_NOW.load(Ordering::Relaxed) == 0 || *(MAGIC_FREEZE_STUCK_BODIES as *const u8) == 0 {
        return false;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    let b2 = if grid_world != 0 { rd(grid_world + GW_B2WORLD) } else { 0 };
    b2 != 0 && *((b2 + B2_STEP_MS) as *const f64) > *(MAGIC_THREAD_MAX_WAIT_MS as *const f32) as f64
}

/// The tick of the frame about to be simulated asks for the stuck-body freeze (some player
/// reported a slow Box2D step), or not.
pub fn set_box2d_freeze(on: bool) {
    let flag = FREEZE_STUCK_NOW.load(Ordering::Relaxed);
    if flag != 0 {
        unsafe { *(flag as *mut u8) = on as u8 };
    }
}

/// PlayerCollisionSystem (FUN_00bfa850, playercollision_system.cpp): while a player is stuck
/// in the ground (stuck_in_ground_counter > 10) and has its collision body, every 10 frames
/// it wakes the Box2D bodies of an AABB around it: `call FUN_009d0d40` (b2 broadphase
/// QueryAABB; [esp+4] callback, [esp+8] the b2AABB of four doubles). The code calls the
/// CharacterDataComponent getters of collision_aabb_min_x/min_y/max_x/max_y, but only one
/// corner is stored, into lowerBound: `pos + max + 3` (Box2D coordinates). upperBound is
/// never written: it is whatever an earlier call left on the game thread's stack. Which
/// sleeping bodies woke (often every body below and right of the player, across the loaded
/// world) depended on that client's earlier code paths, e.g. its own input.
const STUCK_WAKE_QUERY_CALL: u32 = 0x00bf_b2f5;
const B2_QUERY_AABB_FN: u32 = 0x009d_0d40;
/// `pos + 3` / `pos - 3`: the 3 px margin (float) of the stored corner.
const STUCK_WAKE_MARGIN: u32 = 0x0105_3ae8;
/// Pixel -> Box2D: `(x - ox) / sx`, `(y - oy) / sy` (doubles).
const B2_ORIGIN_X: u32 = 0x0120_7b28;
const B2_ORIGIN_Y: u32 = 0x0120_7b30;
const B2_SCALE_X: u32 = 0x0120_7b38;
const B2_SCALE_Y: u32 = 0x0120_7b40;
/// CharacterDataComponent LensValue<float> collision_aabb_* (base value +0, override +4 set
/// at frame +8; the getter FUN_00898180 returns the override while it is under 2 frames old).
const CDATA_AABB_MIN_X: u32 = 0x4c;
const CDATA_AABB_MIN_Y: u32 = 0x64;

/// The query gets the box the code evidently meant: lowerBound `pos + min - 3`, upperBound
/// the stored corner `pos + max + 3`, i.e. the player's collision box grown by 3 px, with the
/// engine's float/double arithmetic. The stub runs at the call with EDI = the entity
/// (position +0x70/+0x74) and ESI = its CharacterDataComponent.
unsafe fn patch_stuck_wake_aabb(ex: &mut Exec) -> Result<(), PatchError> {
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(STUCK_WAKE_QUERY_CALL + 5, B2_QUERY_AABB_FN));
    expect(STUCK_WAKE_QUERY_CALL, &call)?;
    let cb = ((stuck_wake_aabb as unsafe extern "C" fn(*mut f64, u32, u32)) as u32).to_le_bytes();
    let stub_at = ex.base + ex.used;
    // pushad; push edi; push esi; push dword [esp+0x30] (the AABB); mov eax,cb; call eax;
    // add esp,12; popad; jmp FUN_009d0d40
    let mut stub = vec![0x60, 0x57, 0x56, 0xFF, 0x74, 0x24, 0x30, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x0C, 0x61, 0xE9];
    stub.extend_from_slice(&rel32(stub_at + stub.len() as u32 + 4, B2_QUERY_AABB_FN));
    let at = ex.put(&stub);
    let mut site = vec![0xE8];
    site.extend_from_slice(&rel32(STUCK_WAKE_QUERY_CALL + 5, at));
    write_code(STUCK_WAKE_QUERY_CALL, &site)
}

/// See [`patch_stuck_wake_aabb`]. `aabb`: lower.x, lower.y, upper.x, upper.y.
unsafe extern "C" fn stuck_wake_aabb(aabb: *mut f64, cdata: u32, entity: u32) {
    let frame = rd(rd(GAME_GLOBAL_PTR)) as i32;
    let lens = |o: u32| {
        let l = cdata + o;
        if frame.wrapping_sub(rd(l + 8) as i32) <= 1 {
            *((l + 4) as *const f32)
        } else {
            *(l as *const f32)
        }
    };
    let margin = *(STUCK_WAKE_MARGIN as *const f32);
    let px = *((entity + 0x70) as *const f32);
    let py = *((entity + 0x74) as *const f32);
    let d = |a: u32| *(a as *const f64);
    let lower_x = (((lens(CDATA_AABB_MIN_X) + px) - margin) as f64 - d(B2_ORIGIN_X)) / d(B2_SCALE_X);
    let lower_y = (((lens(CDATA_AABB_MIN_Y) + py) - margin) as f64 - d(B2_ORIGIN_Y)) / d(B2_SCALE_Y);
    let (stale_x, stale_y) = (*aabb.add(2), *aabb.add(3));
    *aabb.add(2) = *aabb;
    *aabb.add(3) = *aabb.add(1);
    *aabb = lower_x;
    *aabb.add(1) = lower_y;
    if std::env::var_os("NOITA_ONLINE_WAKEWATCH").is_some() {
        crate::wakewatch::write_line(&format!(
            "{frame} stuck_wake entity_pos={px},{py} stack_upper={stale_x:e},{stale_y:e} aabb={lower_x},{lower_y},{},{}",
            *aabb.add(2),
            *aabb.add(3)
        ));
    }
}

/// GridWorld: every grid physics body (`std::vector<b2Body*>` at +0x560, in creation order)
/// and mFreeSolids (+0x59c, `std::map<b2Body*, std::vector<Cell*>>`: cells removed from a
/// body this frame, grouped by body; MSVC tree head node, value at node+0x10).
const GW_BODIES: u32 = 0x560;
const GW_FREE_SOLIDS: u32 = 0x59c;
/// FUN_0071d020 loop head: `mov esi,[ebx+0x59c]; mov eax,[ebx+0x564]; mov ecx,[ebx+0x560];
/// mov esi,[esi]` (esi = the map's begin node, ebx = GridWorld).
const FREE_SOLIDS_PICK: u32 = 0x0071_d058;
const FREE_SOLIDS_PICK_ORIG: [u8; 20] =
    [0x8B, 0xB3, 0x9C, 0x05, 0x00, 0x00, 0x8B, 0x83, 0x64, 0x05, 0x00, 0x00, 0x8B, 0x8B, 0x60, 0x05, 0x00, 0x00, 0x8B, 0x36];

/// FUN_0071d020 (inside the window, after the grid update) rebuilds every body that lost
/// cells this frame (FUN_0071e120 -> FUN_0071d230: the old body is removed from the body
/// vector, the remaining cells become new bodies). It takes mFreeSolids' entries in key
/// order, i.e. by b2Body *heap address*, which differs between clients. When an explosion
/// broke several bodies in one frame, the new bodies landed at different body-vector
/// positions; the next frame's body->grid pass (FUN_00718d50, sorted by vector position)
/// then placed them in a different order, so displaced cells became particles in a different
/// order and the same RNG draws went to different particles (seed 1717171717, f=1382).
/// The loop now takes the entry whose body comes first in the body vector.
unsafe fn patch_free_solids_order(ex: &mut Exec) -> Result<(), PatchError> {
    expect(FREE_SOLIDS_PICK, &FREE_SOLIDS_PICK_ORIG)?;
    let f = ((free_solid_next as extern "C" fn(u32) -> u32) as u32).to_le_bytes();
    // push ebx; mov eax,free_solid_next; call eax; add esp,4; mov esi,eax; ret
    let stub = ex.put(&[0x53, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x8B, 0xF0, 0xC3]);
    // mov esi,[ebx+0x59c] -> call stub; nop (eax, ecx and edx are reloaded right after)
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(FREE_SOLIDS_PICK + 5, stub));
    call.push(0x90);
    write_code(FREE_SOLIDS_PICK, &call)?;
    // mov esi,[esi] -> nop; nop
    write_code(FREE_SOLIDS_PICK + 18, &[0x90, 0x90])
}

/// The mFreeSolids node to rebuild next: the one whose body has the lowest index in the
/// body vector (identical on every client). Bodies missing from the vector (the engine's
/// "body not found" error path) come last, in map order.
extern "C" fn free_solid_next(grid_world: u32) -> u32 {
    unsafe {
        let head = rd(grid_world + GW_FREE_SOLIDS);
        let (begin, end) = (rd(grid_world + GW_BODIES), rd(grid_world + GW_BODIES + 4));
        let mut index = std::collections::HashMap::new();
        for (i, p) in (begin..end).step_by(4).enumerate() {
            index.entry(rd(p)).or_insert(i as u32);
        }
        let key = |node: u32| index.get(&rd(node + 0x10)).copied().unwrap_or(u32::MAX);
        // In-order walk from the begin node (vanilla's pick); a strictly lower key wins, so
        // map order breaks ties. Nil leaves point at the head (_Isnil at +0xd).
        let mut best = rd(head);
        let mut best_key = key(best);
        let mut stack = Vec::new();
        let mut n = rd(head + 4);
        loop {
            while n != head && *((n + 0xd) as *const u8) == 0 {
                stack.push(n);
                n = rd(n);
            }
            let Some(m) = stack.pop() else { break };
            let k = key(m);
            if k < best_key {
                (best, best_key) = (m, k);
            }
            n = rd(m + 8);
        }
        best
    }
}

/// The pathfinding job (FUN_00861a50, launched by DeathMatch::Update at 0x006b2be1, i.e.
/// before the simulation window, and run inline by [`patch_inline_async`]), from the
/// `PATHFINDING_DISTANCE_FIELD_ENABLED` test to the clearance-refresh step (exclusive):
/// `cmp byte [state+0x14],0; jz ..` (cycle finished: state zeroed), `mov eax,[state+0x18];
/// test eax,eax; jnz step` (mid-cycle), else capture `[job]` into state+0 and the camera
/// centre `trunc(w * 0.5 + x)`, `trunc(h * 0.5 + y)` (WorldData+0 rect) into state+4/+8;
/// `mov ecx,state; call FUN_008605e0` (the step), then 0x00861b07.
const PF_REFRESH_SITE: u32 = 0x0086_1a7b;
const PF_REFRESH_ORIG: [u8; 140] = [
    0x80, 0x3d, 0x34, 0x85, 0x20, 0x01, 0x00, 0x74, 0x2b, 0x0f, 0x57, 0xc0, 0x89, 0x74, 0x24, 0x60, 0xc6, 0x44, 0x24, 0x64,
    0x00, 0x33, 0xc0, 0xf3, 0x0f, 0x7f, 0x05, 0x20, 0x85, 0x20, 0x01, 0xa3, 0x38, 0x85, 0x20, 0x01, 0xf3, 0x0f, 0x7e, 0x44,
    0x24, 0x60, 0x66, 0x0f, 0xd6, 0x05, 0x30, 0x85, 0x20, 0x01, 0xeb, 0x05, 0xa1, 0x38, 0x85, 0x20, 0x01, 0x85, 0xc0, 0x75,
    0x45, 0x8b, 0x07, 0xa3, 0x20, 0x85, 0x20, 0x01, 0xe8, 0xec, 0x80, 0xbd, 0xff, 0x8b, 0x40, 0x0c, 0xf3, 0x0f, 0x10, 0x50,
    0x08, 0xf3, 0x0f, 0x10, 0x48, 0x0c, 0xf3, 0x0f, 0x59, 0x15, 0x1c, 0x36, 0x05, 0x01, 0xf3, 0x0f, 0x59, 0x0d, 0x1c, 0x36,
    0x05, 0x01, 0xf3, 0x0f, 0x58, 0x10, 0xf3, 0x0f, 0x58, 0x48, 0x04, 0xf3, 0x0f, 0x2c, 0xc2, 0xf3, 0x0f, 0x2c, 0xc9, 0xa3,
    0x24, 0x85, 0x20, 0x01, 0x89, 0x0d, 0x28, 0x85, 0x20, 0x01, 0xb9, 0x20, 0x85, 0x20, 0x01, 0xe8, 0xd9, 0xea, 0xff, 0xff,
];
const PF_REFRESH_DONE: u32 = 0x0086_1b07;
/// Clearance-refresh state (FUN_008605e0's coroutine, 0x1c bytes): +0 pathfinding chunk
/// table, +4/+8 centre (int), +0xc/+0x10 progress in the current chunk, +0x14 cycle finished
/// (byte), +0x18 line (0 = start a cycle).
const PF_REFRESH_STATE: u32 = 0x0120_8520;
const PF_REFRESH_STATE_LEN: usize = 0x1c;
const PF_REFRESH_STEP_FN: u32 = 0x0086_05e0;
/// FUN_00860150's chunk scratch (pointer to 134×134 solid flags, filled when a chunk's
/// refresh starts, read by its second half one frame later; allocated on first use).
const PF_REFRESH_SCRATCH_PTR: u32 = 0x0122_70c8;
const PF_REFRESH_SCRATCH_LEN: usize = 0x4624;

/// View centres of the last simulation window (slot order), for the clearance refresh.
static PF_FOCI: std::sync::Mutex<Vec<(f32, f32)>> = std::sync::Mutex::new(Vec::new());

/// One player's refresh cycle: the coroutine state and, mid-chunk, its scratch.
struct PfRefresh {
    state: [u8; PF_REFRESH_STATE_LEN],
    scratch: Vec<u8>,
}

/// Per-player refresh cycles (slot order), used with two or more players.
static PF_REFRESH: std::sync::Mutex<Vec<PfRefresh>> = std::sync::Mutex::new(Vec::new());

/// With PATHFINDING_DISTANCE_FIELD_ENABLED (on by default) the pathfinding job refreshes the
/// node clearance field (the byte at pf_chunk + 0x80009 + 3 * node, which the search reads as
/// the room a mover has; nothing else computes it) of the 2×2 pathfinding chunks around the
/// camera centre, one chunk per two frames (FUN_008605e0 -> FUN_00860150), and starts the
/// next cycle when done. The job runs before the simulation window, so that camera is each
/// client's *render* camera: clients refreshed different chunks at different frames, and an
/// enemy whose path search ran in between got a different path (seed 1404389217, f=1554:
/// rat 969's next path node). The cycle now centres on every player's view (the view
/// centres, i.e. cameras, published by the previous window): with one player exactly
/// vanilla's single cycle; with more, every player runs its own cycle each frame (slot order,
/// own coroutine state and chunk scratch), so every player's surroundings are refreshed at
/// vanilla's rate.
unsafe fn patch_pathfinding_refresh(ex: &mut Exec) -> Result<(), PatchError> {
    expect(PF_REFRESH_SITE, &PF_REFRESH_ORIG)?;
    let f = ((pf_refresh as extern "C" fn(u32)) as u32).to_le_bytes();
    // Site: call stub; jmp done. Stub: push edi (the job); mov eax,pf_refresh; call eax;
    // add esp,4; ret (ESI = 0 and EDI stay as the code after expects them; EAX/ECX/EDX and
    // the XMM registers are dead).
    let stub = ex.put(&[0x57, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0xC3]);
    let mut code = vec![0xE8];
    code.extend_from_slice(&rel32(PF_REFRESH_SITE + 5, stub));
    code.push(0xE9);
    code.extend_from_slice(&rel32(PF_REFRESH_SITE + 10, PF_REFRESH_DONE));
    write_code(PF_REFRESH_SITE, &code)
}

/// One refresh step of the cycle in [`PF_REFRESH_STATE`] around (x, y), as vanilla runs it.
unsafe fn pf_refresh_step(job: u32, (x, y): (f32, f32)) {
    let state = PF_REFRESH_STATE as *mut u8;
    if *state.add(0x14) != 0 {
        std::ptr::write_bytes(state, 0, PF_REFRESH_STATE_LEN);
    }
    if rd(PF_REFRESH_STATE + 0x18) == 0 {
        *(PF_REFRESH_STATE as *mut u32) = rd(job);
        *((PF_REFRESH_STATE + 4) as *mut i32) = x as i32;
        *((PF_REFRESH_STATE + 8) as *mut i32) = y as i32;
    }
    let step: extern "fastcall" fn(u32) = std::mem::transmute(PF_REFRESH_STEP_FN as usize);
    step(PF_REFRESH_STATE);
}

/// The job's clearance refresh around the players (see [`patch_pathfinding_refresh`]).
extern "C" fn pf_refresh(job: u32) {
    unsafe {
        let foci = PF_FOCI.lock().unwrap_or_else(|p| p.into_inner());
        if foci.len() <= 1 {
            pf_refresh_step(job, foci.first().copied().unwrap_or((0.0, 0.0)));
            return;
        }
        let mut cycles = PF_REFRESH.lock().unwrap_or_else(|p| p.into_inner());
        if cycles.is_empty() {
            // The single cycle so far continues as player 1's.
            let mut state = [0u8; PF_REFRESH_STATE_LEN];
            std::ptr::copy_nonoverlapping(PF_REFRESH_STATE as *const u8, state.as_mut_ptr(), PF_REFRESH_STATE_LEN);
            cycles.push(PfRefresh { state, scratch: Vec::new() });
        }
        cycles.resize_with(foci.len(), || PfRefresh { state: [0; PF_REFRESH_STATE_LEN], scratch: Vec::new() });
        let mid_chunk = |s: &[u8; PF_REFRESH_STATE_LEN]| u32::from_le_bytes([s[0x10], s[0x11], s[0x12], s[0x13]]) != 0;
        for (cycle, &focus) in cycles.iter_mut().zip(foci.iter()) {
            std::ptr::copy_nonoverlapping(cycle.state.as_ptr(), PF_REFRESH_STATE as *mut u8, PF_REFRESH_STATE_LEN);
            let scratch = rd(PF_REFRESH_SCRATCH_PTR) as *mut u8;
            if mid_chunk(&cycle.state) && !scratch.is_null() && cycle.scratch.len() == PF_REFRESH_SCRATCH_LEN {
                std::ptr::copy_nonoverlapping(cycle.scratch.as_ptr(), scratch, PF_REFRESH_SCRATCH_LEN);
            }
            pf_refresh_step(job, focus);
            std::ptr::copy_nonoverlapping(PF_REFRESH_STATE as *const u8, cycle.state.as_mut_ptr(), PF_REFRESH_STATE_LEN);
            let scratch = rd(PF_REFRESH_SCRATCH_PTR) as *const u8;
            if mid_chunk(&cycle.state) && !scratch.is_null() {
                cycle.scratch.resize(PF_REFRESH_SCRATCH_LEN, 0);
                std::ptr::copy_nonoverlapping(scratch, cycle.scratch.as_mut_ptr(), PF_REFRESH_SCRATCH_LEN);
            }
        }
    }
}

/// Publish this window's view centres (slot order) for the clearance refresh.
pub fn pathfinding_foci(foci: &[(f32, f32)]) {
    *PF_FOCI.lock().unwrap_or_else(|p| p.into_inner()) = foci.to_vec();
}

static FREEZE_LAST: std::sync::Mutex<Vec<(f32, f32)>> = std::sync::Mutex::new(Vec::new());

/// Publish this frame's view centres (slot order) for the far-body freeze test, and run
/// the freeze pass whenever a view moved more than 512 px since the last pass.
///
/// # Safety
/// Game thread, inside the simulation window, world initialised.
pub unsafe fn focus_freeze_step(foci: &[(f32, f32)]) {
    let arr = FREEZE_FOCI_ARR.load(Ordering::Relaxed);
    let n_at = FREEZE_FOCI_N.load(Ordering::Relaxed);
    if arr == 0 || n_at == 0 {
        return;
    }
    let foci = &foci[..foci.len().min(MAX_FOCI)];
    for (i, &(x, y)) in foci.iter().enumerate() {
        *((arr + 8 * i as u32) as *mut f32) = x;
        *((arr + 8 * i as u32 + 4) as *mut f32) = y;
    }
    *(n_at as *mut u32) = foci.len() as u32;
    let mut last = FREEZE_LAST.lock().unwrap_or_else(|p| p.into_inner());
    let moved = last.len() != foci.len()
        || foci.iter().zip(last.iter()).any(|(a, b)| {
            let (dx, dy) = (a.0 - b.0, a.1 - b.1);
            dx * dx + dy * dy > 262_144.0
        });
    if !moved || foci.is_empty() {
        return;
    }
    *last = foci.to_vec();
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world == 0 {
        return;
    }
    let (x, y) = foci[0];
    let rect = [x as i32 - 213, y as i32 - 120, 427, 240];
    let pass: extern "fastcall" fn(u32, u32, *const i32) = std::mem::transmute(FREEZE_PASS_FN as usize);
    pass(grid_world, 0, rect.as_ptr());
}

/// New run (`no_next_run`, after the old world's teardown): the per-world state of the patches
/// above starts as in a fresh process, identically on every client (they left the old world
/// at different frames): the chunk gate's job ages and submission frames, the camera loads
/// (vanilla again until the new world's first window), the view loads, the eviction schedule,
/// the simulation RNG bank (reseeded by `fix_seeds`), the armed early window, the freeze foci
/// and the pathfinding clearance cycles, including the engine's own cycle state
/// ([`PF_REFRESH_STATE`], a static that the new game keeps: it holds the old world's
/// pathfinding chunk table and each client's progress in its cycle).
///
/// The engine's GridWorld outlives the new game as well: GameGlobal+0x10 and WorldData+0x44
/// keep pointing at the same object, which only a fresh process constructs (`FUN_00717c50`,
/// via `FUN_006f1230`); the rebuild only clears parts of it. Three of its fields kept what each
/// client's old world left and fed the new world's simulation (run 1 of a 2-player restart
/// differed from frame 20 on): its RNG (+4: drawn by the falling-sand update, seeded once at
/// construction from the platform RNG, `rng ^ 0xc`), its frame counter (+0x4fc: the creation
/// frame of generated fire and liquid cells, `chunk_frame`), and the last camera load rect
/// (+0x4a8, the base of the look-ahead load: the rebuild runs with camera loads off, +0x5cc, so
/// it still held each client's last camera at its teardown, and the first window's view loads
/// look ahead from it). GameGlobal::Update (`FUN_006363b0`) counts the world frames (+4) beside
/// the frame number (+0), and the new game resets only the latter. They get their construction
/// values: frames 0, the rect GridWorld init (`FUN_007184d0`) starts from, and the RNG from the
/// new seed's platform RNG ([`fix_seeds`]). The same holds for the grid update's own statics
/// ([`GRID_WAKE_LIST`], [`GRID_PASS_ORDER`]) and the GridThreadImpl (GridWorld+0x5d0, made by
/// GridWorld init) holding the wake cursor: each client's tiles woke at its own place in the
/// old world's shuffled wake list, and its checkerboard passes ran in the order its old world
/// left (run 1 differed from frame 8 on: another tile woken, then frame 10's first active tiles
/// passed in another order). The two lists are emptied (the first grid update rebuilds them
/// exactly as on first use, shuffled with the new run's `rand`) and the cursor is 0. The game
/// thread's Park–Miller state ([`THREAD_PM_FN`]) is a thread's, not the world's: it gets the
/// value the thread's first call gives it (run 1 differed at frame 573: a verlet link broke
/// into a particle on one client only).
///
/// # Safety
/// Game thread, between the old world's teardown and the new world's creation.
pub unsafe fn reset_run() {
    *CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner()) = None;
    CHUNK_SUBMIT_FRAMES.lock().unwrap_or_else(|p| p.into_inner()).clear();
    DEFERRED_TILES.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let flag = CAMERA_LOADS_FLAG.load(Ordering::Relaxed);
    if flag != 0 {
        *(flag as *mut u8) = 1;
    }
    *VIEW_LOADS.lock().unwrap_or_else(|p| p.into_inner()) = ViewLoads { start: None, last: Vec::new() };
    *EVICT.lock().unwrap_or_else(|p| p.into_inner()) = (0, 0.0, false);
    SIM_BANK.with(|b| *b.borrow_mut() = None);
    IN_WINDOW.store(false, Ordering::Relaxed);
    *ARMED_CAMERA.lock().unwrap_or_else(|p| p.into_inner()) = None;
    FREEZE_LAST.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let n = FREEZE_FOCI_N.load(Ordering::Relaxed);
    if n != 0 {
        *(n as *mut u32) = 0;
    }
    set_box2d_freeze(false);
    PF_FOCI.lock().unwrap_or_else(|p| p.into_inner()).clear();
    PF_REFRESH.lock().unwrap_or_else(|p| p.into_inner()).clear();
    std::ptr::write_bytes(PF_REFRESH_STATE as *mut u8, 0, PF_REFRESH_STATE_LEN);
    let gg = rd(GAME_GLOBAL_PTR);
    if gg != 0 {
        *((gg + GG_WORLD_FRAMES) as *mut u32) = 0;
    }
    let gw = grid_world_ptr();
    if gw != 0 {
        *((gw + GW_FRAME) as *mut u32) = 0;
        std::ptr::copy_nonoverlapping(GW_CAMERA_LOAD_RECT_INIT.as_ptr(), (gw + GW_CAMERA_LOAD_RECT) as *mut i32, 4);
        GRID_RNG_RESEED.store(true, Ordering::Relaxed);
        let imp = rd(gw + 0x5d0);
        if imp != 0 {
            *((imp + GRID_IMPL_WAKE_CURSOR) as *mut u32) = 0;
        }
    }
    for list in [GRID_WAKE_LIST, GRID_PASS_ORDER] {
        *((list + 4) as *mut u32) = rd(list);
    }
    let thread_pm: extern "C" fn() -> *mut f64 = std::mem::transmute(THREAD_PM_FN as usize);
    let pm = thread_pm();
    if !pm.is_null() {
        *pm = *(THREAD_PM_INIT as *const f64);
    }
}
