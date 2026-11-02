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

unsafe fn rd(a: u32) -> u32 {
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
    /// finished-but-too-young jobs hidden from this frame's integration
    hidden: Vec<u32>,
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
    let frame = rd(gg);
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
/// [`chunk_prewait`]); finished jobs that are still too young are hidden for this frame.
extern "C" fn chunk_gate_pre() {
    unsafe {
        let gg = rd(GAME_GLOBAL_PTR);
        let frame = if gg != 0 { rd(gg) } else { 0 };
        let jobs = chunk_jobs();
        let mut g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
        let gate = g.get_or_insert_with(|| ChunkGate { first_seen: Default::default(), hidden: Vec::new() });
        gate.first_seen.retain(|k, _| jobs.iter().any(|(jk, _)| jk == k));
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
                }
            } else if job_done(job) {
                *((job + 0xc8) as *mut i32) = 0;
                gate.hidden.push(job);
            }
        }
    }
}

pub static CHUNK_GATE_FALLBACKS: AtomicU32 = AtomicU32::new(0);

/// After integration: un-hide the young finished jobs for later frames.
extern "C" fn chunk_gate_post() {
    unsafe {
        let mut g = CHUNK_GATE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(gate) = g.as_mut() {
            for job in gate.hidden.drain(..) {
                *((job + 0xc8) as *mut i32) = 1;
            }
        }
    }
}

unsafe fn patch_chunk_wait(ex: &mut Exec) -> Result<(), PatchError> {
    let orig_rel = rel32(CHUNK_INTEGRATE_CALL + 5, CHUNK_INTEGRATE_FN);
    expect(CHUNK_INTEGRATE_CALL, &[0xE8, orig_rel[0], orig_rel[1], orig_rel[2], orig_rel[3]])?;
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

/// Async jobs (the Box2D step, cosmetic particles, chunk saving, ... — everything but chunk
/// generation, see [`patch_threaded_chunkgen`]) run synchronously at submission: the
/// Box2D step would otherwise overlap the next frame's simulation.
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
unsafe fn redirect_call(site: u32, orig: u32, new_target: u32) -> Result<(), PatchError> {
    let orig_rel = rel32(site + 5, orig);
    expect(site, &[0xE8, orig_rel[0], orig_rel[1], orig_rel[2], orig_rel[3]])?;
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(site + 5, new_target));
    write_code(site, &call)
}

/// Keep world generation on the engine's worker threads (it is expensive: 17-36 ms per
/// chunk) while everything else runs inline: chunk jobs get an unpatched copy of the
/// async submit, and their only shared-RNG write is removed. Integration stays
/// frame-deterministic through the chunk-wait barrier. Must run before
/// [`patch_inline_jobs`] (it copies the original submit routine).
unsafe fn patch_threaded_chunkgen(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CELL_CTOR_LCG_STORE, &[0xA3, 0x1C, 0x74, 0x20, 0x01])?;
    expect(POOL_ASYNC_FN, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x18])?;
    let copy = copy_function(ex, POOL_ASYNC_FN, POOL_ASYNC_LEN, &POOL_ASYNC_RELCALLS)?;
    redirect_call(CHUNKGEN_SUBMIT_CALL, POOL_ASYNC_FN, copy)?;
    write_code(CELL_CTOR_LCG_STORE, &[0x90; 5])
}

/// Group job submit (FUN_0084c240): 0xd4 bytes, rel32 calls at +0x64 and +0x92.
const POOL_GROUP_FN: u32 = 0x0084_c240;
const POOL_GROUP_LEN: usize = 0xd4;
const POOL_GROUP_RELCALLS: [usize; 2] = [0x64, 0x92];
/// The falling-sand tile dispatches inside the grid update (FUN_00726340).
const GRID_TILE_SUBMIT_CALLS: [u32; 3] = [0x0072_7d5a, 0x0072_8051, 0x0072_82b5];

/// The falling-sand grid's 64x64 tiles run in parallel deterministically (each tile owns
/// its RNG and the four checkerboard passes never let tiles race), so they keep the
/// worker threads; other pool jobs run inline. Must run before [`patch_inline_jobs`].
unsafe fn patch_threaded_grid(ex: &mut Exec) -> Result<(), PatchError> {
    expect(POOL_GROUP_FN, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14])?;
    let copy = copy_function(ex, POOL_GROUP_FN, POOL_GROUP_LEN, &POOL_GROUP_RELCALLS)?;
    for site in GRID_TILE_SUBMIT_CALLS {
        redirect_call(site, POOL_GROUP_FN, copy)?;
    }
    Ok(())
}

static APPLIED: AtomicU32 = AtomicU32::new(0);

/// Install all determinism patches (idempotent). Returns the applied-set bitmask (bit i =
/// patch i of [`PATCHES`]). For experiments, `NOITA_ONLINE_PATCH_OFF=name,name` skips
/// patches by name (never set in normal play: every patch is needed for determinism).
///
/// # Safety
/// Must run inside noita.exe build 0x6794ee3c, on the game thread, before the world exists.
pub unsafe fn apply_all() -> Result<u32, PatchError> {
    if APPLIED.load(Ordering::Relaxed) != 0 {
        return Ok(APPLIED.load(Ordering::Relaxed) & !(1 << 31));
    }
    let off = std::env::var("NOITA_ONLINE_PATCH_OFF").unwrap_or_default();
    let skip = |name: &str| off.split(',').any(|s| s.trim() == name);
    let mut ex = Exec::new()?;
    let mut mask = 0u32;
    for (i, (name, patch)) in PATCHES.iter().enumerate() {
        if skip(name) {
            continue;
        }
        patch(&mut ex).map_err(|e| PatchError(format!("{name}: {}", e.0)))?;
        mask |= 1 << i;
    }
    if std::env::var_os("NOITA_ONLINE_PTRACE").is_some() {
        install_particle_trace(&mut ex)?;
    }
    APPLIED.store(mask | 1 << 31, Ordering::Relaxed);
    Ok(mask)
}

/// Particle slot allocators (every grid particle is created through one of them); their
/// first 10 bytes `push ebp; mov ebp,esp; cmp byte [0x01204ba4],0` are relocatable.
const PARTICLE_SLOT_FNS: [u32; 2] = [0x0071_7400, 0x0071_74e0];
const PARTICLE_SLOT_PROLOGUE: [u8; 10] = [0x55, 0x8B, 0xEC, 0x80, 0x3D, 0xA4, 0x4B, 0x20, 0x01, 0x00];

/// Diagnostics: `NOITA_ONLINE_PTRACE=from-to` logs every grid-particle creation in that frame
/// range (thread, allocator, EBP-chain return addresses, list sizes) to
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
    Ok(())
}

extern "C" fn particle_trace(tag: u32, regs: *const u32) {
    use std::io::Write;
    static LOG: std::sync::LazyLock<Option<(u32, u32, std::sync::Mutex<std::fs::File>)>> = std::sync::LazyLock::new(|| {
        let v = std::env::var("NOITA_ONLINE_PTRACE").ok()?;
        let (a, b) = v.split_once('-')?;
        let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
        let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
        let f = std::fs::File::create(std::path::Path::new(&out).join(format!("ptrace_{inst}.txt"))).ok()?;
        Some((a.parse().ok()?, b.parse().ok()?, std::sync::Mutex::new(f)))
    });
    unsafe {
        let Some((a, b, file)) = LOG.as_ref() else { return };
        let gg = rd(GAME_GLOBAL_PTR);
        let frame = if gg != 0 { rd(gg) } else { 0 };
        if frame < *a || frame > *b {
            return;
        }
        // pushad block: edi esi ebp esp ebx edx ecx eax
        let esp = *regs.add(3);
        let mut ebp = *regs.add(2);
        let gp = *regs.add(6);
        let mut line = format!(
            "{frame} t{} a{tag} n={}/{} ret={:08x}",
            GetCurrentThreadId(),
            (rd(gp + 0x38).wrapping_sub(rd(gp + 0x34))) / 0x70,
            (rd(gp + 0x50).wrapping_sub(rd(gp + 0x4c))) / 0x70,
            rd(esp)
        );
        for _ in 0..8 {
            if ebp <= esp || ebp > esp + 0x10_0000 || ebp & 3 != 0 {
                break;
            }
            line.push_str(&format!(" {:08x}", rd(ebp + 4)));
            ebp = rd(ebp);
        }
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}");
        }
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
    ("threaded_grid", patch_threaded_grid),
    ("inline_group", |_| unsafe { patch_inline_group() }),
    ("inline_async", |_| unsafe { patch_inline_async() }),
    ("particle_init", |_| unsafe { patch_particle_init() }),
    ("camera_loads", |_| unsafe { patch_camera_loads() }),
    ("fixed_dt", patch_fixed_dt),
    ("focus_freeze", patch_focus_freeze),
    ("cosmetic_particles", |_| unsafe { patch_cosmetic_particles() }),
    ("early_window", patch_early_window),
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
    let rs = RAND_STATE_ADDR.load(Ordering::Relaxed);
    if rs != 0 {
        *(rs as *mut u32) = seed.wrapping_mul(0x9e37_79b9) | 1;
    }
    // The simulation RNG bank starts from exactly these values.
    SIM_BANK.with(|b| *b.borrow_mut() = Some(RngBank::capture()));
}

// --- multi-focus streaming, particles, fixed timestep, simulation RNG bank ---------------

/// GridParticles RNG (double), seeded from the platform RNG.
const PARTICLE_RNG: u32 = 0x0122_2388;
/// Particle constructor: `and byte [esi+0x6a],0x02` / `and byte [esi+0x6c],0xfc` keep
/// uninitialised heap bits (they pick the collision world); clear them fully instead.
const PARTICLE_CTOR_AND_6A: u32 = 0x0071_213e;
const PARTICLE_CTOR_AND_6C: u32 = 0x0071_2144;
/// Camera-driven GridWorld::RequestLoad calls (camera rect + look-ahead).
const CAMERA_LOAD_CALLS: [u32; 2] = [0x0072_0210, 0x0072_0290];
/// StreamingKeepAlive: RequestLoad(rect) + keep chunks in rect±256 loaded for 10 frames.
const KEEP_ALIVE_RECT_FN: u32 = 0x0073_e080;
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

/// Chunks are loaded around every player (identically on all clients) instead of around
/// each client's own camera.
unsafe fn patch_camera_loads() -> Result<(), PatchError> {
    for site in CAMERA_LOAD_CALLS {
        if *(site as *const u8) != 0xE8 {
            return Err(PatchError(format!("camera load call at {site:08x} not found")));
        }
    }
    for site in CAMERA_LOAD_CALLS {
        // call RequestLoad (cdecl, 2 args) -> add esp,8; nop; nop
        write_code(site, &[0x83, 0xC4, 0x08, 0x90, 0x90])?;
    }
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

/// Request loading of (and keep alive) the chunks around a focus rectangle this frame.
///
/// # Safety
/// Game thread, inside the simulation window, world initialised.
pub unsafe fn stream_focus(x0: i32, y0: i32, x1: i32, y1: i32) {
    let rect = [x0, y0, x1, y1];
    let f: extern "fastcall" fn(*const i32) = std::mem::transmute(KEEP_ALIVE_RECT_FN as usize);
    f(rect.as_ptr());
}

/// Deterministic replacement for the wall-clock streaming unloader: unload up to `max`
/// loaded chunks (ascending table index) that lie outside every focus rect grown by
/// `margin`. Returns how many were unloaded.
///
/// # Safety
/// Game thread, inside the simulation window, world initialised.
pub unsafe fn unload_far_chunks(foci: &[[i32; 4]], margin: i32, max: u32) -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return 0;
    }
    let world_data = rd(gg + 0x0c);
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world == 0 {
        return 0;
    }
    let chunk_map = grid_world + 0x500;
    let table = rd(chunk_map + 8) as *const u32;
    if table.is_null() {
        return 0;
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
    let mut n = 0;
    for idx in 0..512 * 512u32 {
        if n >= max {
            break;
        }
        if *table.add(idx as usize) == 0 {
            continue;
        }
        let cx = (idx % 512) as i32 - 256;
        let cy = (idx / 512) as i32 - 256;
        let (x0, y0) = (cx * 512, cy * 512);
        let keep = foci.iter().any(|r| {
            x0 < r[2] + margin && x0 + 512 > r[0] - margin && y0 < r[3] + margin && y0 + 512 > r[1] - margin
        });
        if !keep {
            let coords = [cx, cy, idx as i32];
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
/// Simulation camera position at the last window exit; consumed by the early window entry
/// of the next frame ([`early_window_entry`]). Only armed when [`patch_early_window`] is on.
static ARMED_CAMERA: std::sync::Mutex<Option<(f32, f32)>> = std::sync::Mutex::new(None);
static EARLY_WINDOW_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Entering the simulation window: stash the per-client RNG states, load the sim stream.
/// Idempotent within a frame (the window may already have been entered early).
///
/// # Safety
/// Game thread.
pub unsafe fn sim_rng_enter() {
    if IN_WINDOW.swap(true, Ordering::Relaxed) {
        return;
    }
    RENDER_BANK.with(|b| *b.borrow_mut() = Some(RngBank::capture()));
    SIM_BANK.with(|b| {
        if let Some(s) = *b.borrow() {
            s.restore();
        }
    });
    rng_log('<');
}

/// Leaving the simulation window: save the sim stream, put the per-client states back,
/// and arm the next frame's early entry with the current simulation camera.
///
/// # Safety
/// Game thread.
pub unsafe fn sim_rng_leave() {
    if !IN_WINDOW.swap(false, Ordering::Relaxed) {
        return;
    }
    rng_log('>');
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
}

/// DeathMatch::Update, right before the `OnWorldPreUpdate` dispatch loop over every mod's Lua
/// state: `mov edi,[0x01207ea0]` (end of the mod-state vector).
const PRE_UPDATE_DISPATCH: u32 = 0x006b_3201;
/// GameGlobal::SetCameraPos(this = GameGlobal, const float pos[2], bool force), thiscall.
const SET_CAMERA_POS_FN: u32 = 0x0063_6580;
/// GameGlobal: last position requested through SetCameraPos (float x, y).
const GG_CAMERA_REQUEST: u32 = 0xc8;

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
        let Some((x, y)) = ARMED_CAMERA.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
        let gg = rd(GAME_GLOBAL_PTR);
        if gg == 0 {
            return;
        }
        sim_rng_enter();
        let set: extern "thiscall" fn(u32, *const f32, i32) = std::mem::transmute(SET_CAMERA_POS_FN as usize);
        set(gg, [x, y].as_ptr(), 0);
    }
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
    if let Ok(mut f) = file.lock() {
        let _ = writeln!(
            f,
            "{frame} {tag} lcg={:08x} plat={:08x}/{:08x} pm={:.0} part={:.0} rand={:08x} gw={gw_rng:08x}",
            s.lcg, s.plat_a0, s.plat_a4, s.pm, s.particle, s.rand
        );
        if world_data != 0 && grid_world != 0 {
            let fl = |o: u32| *((world_data + o) as *const f32);
            let gi = |o: u32| rd(grid_world + o) as i32;
            let _ = writeln!(
                f,
                "C {frame} {tag} cam={},{},{},{} view={},{},{},{} borders={},{},{},{}",
                fl(0),
                fl(4),
                fl(8),
                fl(12),
                gi(0x498),
                gi(0x49c),
                gi(0x4a0),
                gi(0x4a4),
                gi(0x4a8),
                gi(0x4ac),
                gi(0x4b0),
                gi(0x4b4)
            );
        }
        if tag == '<' && std::env::var_os("NOITA_ONLINE_RNGLOG_TILES").is_some() {
            let _ = f.write_all(grid_tiles_dump(frame).as_bytes());
        }
    }
}

/// Diagnostics: every 64x64 grid tile (tile map in key order) as `T frame x y rng` and the
/// active list in order as `A frame x,y ...`.
unsafe fn grid_tiles_dump(frame: u32) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    let imp = if grid_world != 0 { rd(grid_world + 0x5d0) } else { 0 };
    if imp == 0 {
        return out;
    }
    let mgr = rd(imp + 0x40);
    let head = if mgr != 0 { rd(mgr + 0x14) } else { 0 };
    if head != 0 {
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
            }
            n = rd(m + 8);
        }
    }
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
/// Cosmetic-particle flag computation (`and al,2` imm8) choosing the camera snapshot grid.
const COSMETIC_PARTICLE_FLAG_IMMS: [u32; 2] = [0x0071_6a10, 0x0071_6c52];
const MAX_FOCI: usize = 16;

static FREEZE_FOCI_N: AtomicU32 = AtomicU32::new(0);
static FREEZE_FOCI_ARR: AtomicU32 = AtomicU32::new(0);

/// Body freezing follows the players (foci) instead of each client's render camera: the
/// camera-triggered pass is removed, the distance test uses the nearest focus, and the
/// pass is triggered deterministically by [`focus_freeze_step`].
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

/// Cosmetic particles collide against the real grid instead of a snapshot of the area
/// around each client's render camera (they become real cells when they land).
unsafe fn patch_cosmetic_particles() -> Result<(), PatchError> {
    for imm in COSMETIC_PARTICLE_FLAG_IMMS {
        expect(imm - 4, &[0x32, 0x46, 0x6A, 0x24, 0x02])?;
    }
    for imm in COSMETIC_PARTICLE_FLAG_IMMS {
        write_code(imm, &[0x00])?;
    }
    Ok(())
}

static FREEZE_LAST: std::sync::Mutex<Vec<(f32, f32)>> = std::sync::Mutex::new(Vec::new());

/// Publish this frame's focus centres (slot order) for the far-body freeze test, and run
/// the freeze pass whenever a focus moved more than 512 px since the last pass.
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
