//! Cell explosions decided inside the falling-sand tile jobs.
//!
//! The grid update (`FUN_00726340`) runs each 64×64 tile (`FUN_0072a150`) as a job, on the
//! worker threads ([`crate::patches`] `threaded_grid`) or inline, in four checkerboard
//! passes. A tile is a `GridWorldThreaded` (vtable `0x01017b24`): the cells it updates see
//! it as their world, and it queues their world edits in tile-local vectors that the game
//! thread replays after the passes, tile by tile in active-list order (`FUN_0072a8f0`,
//! `FUN_0072abf0`). Three things on the explosion path escaped that buffering:
//!
//! - **Explosion count.** Burning cells (`FUN_0070fb10`), burning physics bodies (the
//!   CSolidCell update `FUN_0070e060`) and reactions (`FUN_0070ebf0`) explode only while
//!   the world's explosion count (vtable+0x84) is 0. The tile's count (`FUN_0072b430`) is
//!   one process-wide counter (`0x01207e7c`) that every tile's queued explosion
//!   (vtable+0x80, `FUN_0072b3d0`) increments atomically and the flush resets. A cell
//!   therefore exploded, or did not, depending on whether a tile running at the same time
//!   on another thread had already queued an explosion; and the burning check draws the
//!   tile RNG only when the count is 0, so the tile's later cells diverged too.
//! - **Explosion config.** Before queuing, these sites write the explosion's radius and
//!   friends (+4, +8, +0xdc, +0xe0, +0x148, +0x14c) into the material's or reaction's
//!   shared `ConfigExplosion`, which the queue then copies: two tiles exploding the same
//!   material at once could copy each other's values, and every explosion inherited the
//!   fields the last one (from any tile, in thread order) left behind.
//! - **Body explosion positions.** A burning body picks random cells of itself to explode
//!   at with the thread-local Park–Miller RNG (`FUN_0063d1c0`), whose state depends on what
//!   that worker thread ran before.
//!
//! Now (patch `tile_explosions`):
//! - a tile's count is the explosions queued in this frame's earlier passes (sampled by the
//!   game thread at the start of each pass, when no tile runs) plus its own queue, so no
//!   tile sees another tile of the same pass;
//! - on a tile, the three sites write and queue a thread-local copy of the config, taken
//!   when they load its pointer (the shared config is no longer written by tile jobs);
//! - on a tile, the body explosion's RNG is a stream seeded from the tile's RNG state and
//!   the burning cell's position.
//!
//! The real `GridWorld` (game-thread callers such as `FUN_00baac80`) keeps vanilla's
//! behaviour.

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError, GAME_GLOBAL_PTR};
use std::cell::{Cell, UnsafeCell};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

/// `GridWorldThreaded` (grid tile) vtable.
const TILE_VTABLE: u32 = 0x0101_7b24;
/// Its explosion-count slot (vtable+0x84) and the original target, `FUN_0072b430`:
/// `mov eax,[TILE_EXPLOSIONS]; ret`.
const TILE_COUNT_SLOT: u32 = TILE_VTABLE + 0x84;
const TILE_COUNT_FN: u32 = 0x0072_b430;
/// Explosions queued by all tiles since the last flush (`lock xadd` in `FUN_0072b3d0`,
/// reset in the flush `FUN_0072abf0`).
const TILE_EXPLOSIONS: u32 = 0x0120_7e7c;
/// Tile vtable+0x80 (queue an explosion) and its original target `FUN_0072b3d0` (thiscall:
/// config, position `float[2]`; copies both into the tile's queue, returns the count before).
const TILE_QUEUE_SLOT: u32 = TILE_VTABLE + 0x80;
const TILE_QUEUE_FN: u32 = 0x0072_b3d0;
/// A tile's explosion queue: `std::vector<ConfigExplosion + pos>` at tile+0x4fc (begin) /
/// +0x500 (end), 0x174-byte elements.
const TILE_QUEUE_BEGIN: u32 = 0x4fc;
const TILE_QUEUE_END: u32 = 0x500;
const QUEUE_ELEM: u32 = 0x174;

/// Head of the checkerboard pass loop in `FUN_00726340` (run once per pass, before its tiles
/// are dispatched; the previous pass is joined): `cmp byte [USE_CUSTOM_THREADPOOL],0`.
const PASS_HEAD: u32 = 0x0072_8180;
const PASS_HEAD_ORIG: [u8; 7] = [0x80, 0x3D, 0x77, 0x31, 0x15, 0x01, 0x00];

/// `ConfigExplosion` size (its copy constructor `FUN_004b5d60` deep-copies the members).
const CFG_WORDS: usize = 0x174 / 4;

/// Sites loading a shared `ConfigExplosion` pointer just before writing to it and queuing
/// it ([`cfg_load`] knows each site's registers; the world is in EDI at all three).
struct CfgSite {
    at: u32,
    orig: &'static [u8],
    /// The original instruction(s) after the pointer load, re-run by the stub after the
    /// registers are restored.
    tail: &'static [u8],
}

const CFG_SITES: [CfgSite; 3] = [
    // FUN_0070fb10 (burning cell): mov esi,[ecx+0x100] (material's config)
    CfgSite { at: 0x0070_fd5a, orig: &[0x8B, 0xB1, 0x00, 0x01, 0x00, 0x00], tail: &[] },
    // FUN_0070ebf0 (reaction): mov ecx,[ebx+0x3c]; movss xmm3,[0x01053fa4]
    CfgSite {
        at: 0x0070_ee74,
        orig: &[0x8B, 0x4B, 0x3C, 0xF3, 0x0F, 0x10, 0x1D, 0xA4, 0x3F, 0x05, 0x01],
        tail: &[0xF3, 0x0F, 0x10, 0x1D, 0xA4, 0x3F, 0x05, 0x01],
    },
    // FUN_0070e060 (burning body): mov ebx,[eax+0x100] (material's config, may be null)
    CfgSite { at: 0x0070_e1b3, orig: &[0x8B, 0x98, 0x00, 0x01, 0x00, 0x00], tail: &[] },
];

/// In `FUN_0070e060`: `call FUN_0063d1c0` (the calling thread's Park–Miller state, a double)
/// before drawing the explosion positions; EDI = world, ESI = the burning cell.
const BODY_RNG_CALL: u32 = 0x0070_e290;
const THREAD_RNG_FN: u32 = 0x0063_d1c0;

/// Pushad block indices (EDI lowest), after the 8 saved XMM registers.
const EDI: usize = 0;
const ESI: usize = 1;
const EBX: usize = 4;
const ECX: usize = 6;
const EAX: usize = 7;

/// Explosions queued by tiles in this frame's finished passes ([`pass_begin`]).
static COMMITTED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// Per site: this thread's copy of the config the site is about to write and queue.
    static CFG_COPY: UnsafeCell<[[u32; CFG_WORDS]; 3]> = const { UnsafeCell::new([[0; CFG_WORDS]; 3]) };
    /// The body-explosion position RNG of the tile job running on this thread.
    static BODY_RNG: Cell<f64> = const { Cell::new(1.0) };
}

unsafe fn is_tile(world: u32) -> bool {
    world != 0 && rd(world) == TILE_VTABLE
}

/// Tile vtable+0x84: explosions queued in earlier passes of this frame plus the tile's own.
unsafe extern "thiscall" fn tile_explosion_count(tile: u32) -> i32 {
    let own = rd(tile + TILE_QUEUE_END).wrapping_sub(rd(tile + TILE_QUEUE_BEGIN)) / QUEUE_ELEM;
    COMMITTED.load(Ordering::Acquire).wrapping_add(own) as i32
}

/// Start of a checkerboard pass (game thread; the previous pass's jobs are joined).
extern "C" fn pass_begin(_: u32, _: *mut u32) {
    let n = unsafe { std::ptr::read_volatile(TILE_EXPLOSIONS as *const u32) };
    COMMITTED.store(n, Ordering::Release);
}

/// A [`CFG_SITES`] site loads a config pointer: on a tile, the destination register gets
/// this thread's copy of the config instead.
extern "C" fn cfg_load(site: u32, ctx: *mut u32) {
    unsafe {
        let reg = |r: usize| *ctx.add(32 + r);
        let (cfg, dest) = match site {
            0 => (rd(reg(ECX) + 0x100), ESI),
            1 => (rd(reg(EBX) + 0x3c), ECX),
            _ => (rd(reg(EAX) + 0x100), EBX),
        };
        let mut out = cfg;
        if cfg != 0 && is_tile(reg(EDI)) {
            out = CFG_COPY.with(|c| {
                let copy = &mut (*c.get())[site as usize];
                std::ptr::copy_nonoverlapping(cfg as *const u32, copy.as_mut_ptr(), CFG_WORDS);
                copy.as_mut_ptr() as u32
            });
        }
        *ctx.add(32 + dest) = out;
    }
}

/// [`BODY_RNG_CALL`]: on a tile, a Park–Miller state seeded from the tile's RNG (+4) and the
/// cell's position (+0x48, +0x4c); elsewhere the engine's thread-local one.
extern "C" fn body_rng(world: u32, cell: u32) -> u32 {
    unsafe {
        if !is_tile(world) {
            let f: extern "C" fn() -> u32 = std::mem::transmute(THREAD_RNG_FN as usize);
            return f();
        }
        let mut h = rd(world + 4) ^ rd(cell + 0x48).wrapping_mul(0x9e37_79b1) ^ rd(cell + 0x4c).wrapping_mul(0x85eb_ca77);
        h ^= h >> 16;
        h = h.wrapping_mul(0x7feb_352d);
        h ^= h >> 15;
        BODY_RNG.with(|r| {
            r.set((h % 0x7fff_fffe + 1) as f64);
            r.as_ptr() as u32
        })
    }
}

/// `pushad; sub esp,0x80; movups [esp+16i],xmm_i; push esp; push arg; call cb; add esp,8;
/// movups xmm_i,[esp+16i]; add esp,0x80; popad; <tail>; ret`: `cb(arg, block)` may edit the
/// saved registers (block: XMM0..7, then the pushad block).
unsafe fn ctx_stub(ex: &mut Exec, cb: extern "C" fn(u32, *mut u32), arg: u32, tail: &[u8]) -> u32 {
    let mut s = vec![0x60, 0x81, 0xEC, 0x80, 0x00, 0x00, 0x00];
    for i in 0..8u8 {
        s.extend_from_slice(&[0x0F, 0x11, 0x44 | (i << 3), 0x24, i * 16]);
    }
    s.extend_from_slice(&[0x8B, 0xC4, 0x50, 0x68]);
    s.extend_from_slice(&arg.to_le_bytes());
    s.push(0xB8);
    s.extend_from_slice(&(cb as u32).to_le_bytes());
    s.extend_from_slice(&[0xFF, 0xD0, 0x83, 0xC4, 0x08]);
    for i in 0..8u8 {
        s.extend_from_slice(&[0x0F, 0x10, 0x44 | (i << 3), 0x24, i * 16]);
    }
    s.extend_from_slice(&[0x81, 0xC4, 0x80, 0x00, 0x00, 0x00, 0x61]);
    s.extend_from_slice(tail);
    s.push(0xC3);
    ex.put(&s)
}

/// `call <to>` at `site`, padded with NOPs to `len` bytes.
unsafe fn write_call(site: u32, to: u32, len: usize) -> Result<(), PatchError> {
    let mut v = vec![0xE8];
    v.extend_from_slice(&rel32(site + 5, to));
    v.resize(len, 0x90);
    write_code(site, &v)
}

/// The explosion path of tile jobs made independent of thread timing (module docs).
pub(crate) unsafe fn patch_tile_explosions(ex: &mut Exec) -> Result<(), PatchError> {
    expect(TILE_COUNT_SLOT, &TILE_COUNT_FN.to_le_bytes())?;
    expect(TILE_COUNT_FN, &[0xA1, 0x7C, 0x7E, 0x20, 0x01, 0xC3])?;
    expect(PASS_HEAD, &PASS_HEAD_ORIG)?;
    for s in &CFG_SITES {
        expect(s.at, s.orig)?;
    }
    let rng_rel = rel32(BODY_RNG_CALL + 5, THREAD_RNG_FN);
    expect(BODY_RNG_CALL, &[0xE8, rng_rel[0], rng_rel[1], rng_rel[2], rng_rel[3]])?;

    let count: unsafe extern "thiscall" fn(u32) -> i32 = tile_explosion_count;
    write_code(TILE_COUNT_SLOT, &(count as usize as u32).to_le_bytes())?;

    let stub = ctx_stub(ex, pass_begin, 0, &PASS_HEAD_ORIG);
    write_call(PASS_HEAD, stub, PASS_HEAD_ORIG.len())?;

    for (i, s) in CFG_SITES.iter().enumerate() {
        let stub = ctx_stub(ex, cfg_load, i as u32, s.tail);
        write_call(s.at, stub, s.orig.len())?;
    }

    // push esi; push edi; mov eax,body_rng; call eax; add esp,8; ret
    let f = ((body_rng as extern "C" fn(u32, u32) -> u32) as u32).to_le_bytes();
    let stub = ex.put(&[0x56, 0x57, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0xC3]);
    write_call(BODY_RNG_CALL, stub, 5)
}

/// Diagnostics: `NOITA_ONLINE_EXPLOG=a-b` logs, in frames a..b, every explosion a tile job
/// queues (`<frame> q <x> <y> <radius bits> <process-wide count before> <tile x> <tile y>`)
/// and every explosion-count check that answered "not now" (`<frame> c <count> <tile x>
/// <tile y>`), to `$NOITA_ONLINE_OUT/explog_<instance>.txt` in thread order (sort within a
/// frame to compare clients).
static EXPLOG: LazyLock<Option<(u32, u32, Mutex<std::fs::File>)>> = LazyLock::new(|| {
    let v = std::env::var("NOITA_ONLINE_EXPLOG").ok()?;
    let (a, b) = v.split_once('-')?;
    let out = std::env::var("NOITA_ONLINE_OUT").ok()?;
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let f = std::fs::File::create(std::path::Path::new(&out).join(format!("explog_{inst}.txt"))).ok()?;
    Some((a.parse().ok()?, b.parse().ok()?, Mutex::new(f)))
});

/// The count slot's target before [`install_log`] wrapped it.
static LOGGED_COUNT_FN: AtomicU32 = AtomicU32::new(0);

unsafe fn explog(tile: u32, what: std::fmt::Arguments) {
    let Some((a, b, file)) = EXPLOG.as_ref() else { return };
    let gg = rd(GAME_GLOBAL_PTR);
    let frame = if gg != 0 { rd(gg) } else { 0 };
    if (*a..=*b).contains(&frame) {
        let (tx, ty) = (rd(tile + 0x540) as i32, rd(tile + 0x544) as i32);
        let line = format!("{frame} {what} {tx} {ty}\n");
        let _ = file.lock().unwrap_or_else(|p| p.into_inner()).write_all(line.as_bytes());
    }
}

/// Tile vtable+0x80 with [`EXPLOG`]: the engine's queue, then the log line.
unsafe extern "thiscall" fn tile_queue_logged(tile: u32, cfg: u32, pos: *const f32) -> i32 {
    let queue: unsafe extern "thiscall" fn(u32, u32, *const f32) -> i32 = std::mem::transmute(TILE_QUEUE_FN as usize);
    let n = queue(tile, cfg, pos);
    explog(tile, format_args!("q {} {} {:08x} {n}", *pos, *pos.add(1), rd(cfg + 8)));
    n
}

/// Tile vtable+0x84 with [`EXPLOG`]: the installed count, logged when it is not 0.
unsafe extern "thiscall" fn tile_count_logged(tile: u32) -> i32 {
    let count: unsafe extern "thiscall" fn(u32) -> i32 = std::mem::transmute(LOGGED_COUNT_FN.load(Ordering::Relaxed) as usize);
    let n = count(tile);
    if n > 0 {
        explog(tile, format_args!("c {n}"));
    }
    n
}

/// Install [`EXPLOG`] when its variable is set (after `tile_explosions`, or without it).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c.
pub(crate) unsafe fn install_log() -> Result<(), PatchError> {
    if EXPLOG.is_none() {
        return Ok(());
    }
    expect(TILE_QUEUE_SLOT, &TILE_QUEUE_FN.to_le_bytes())?;
    let f: unsafe extern "thiscall" fn(u32, u32, *const f32) -> i32 = tile_queue_logged;
    write_code(TILE_QUEUE_SLOT, &(f as usize as u32).to_le_bytes())?;
    LOGGED_COUNT_FN.store(rd(TILE_COUNT_SLOT), Ordering::Relaxed);
    let f: unsafe extern "thiscall" fn(u32) -> i32 = tile_count_logged;
    write_code(TILE_COUNT_SLOT, &(f as usize as u32).to_le_bytes())
}
