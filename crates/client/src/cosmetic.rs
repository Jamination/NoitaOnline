//! Particles tied to the camera, for every player's view.
//!
//! Vanilla simulates some particles only on screen or around the camera:
//! - Particle creation with the "view-bound" flag returns early outside the camera rect
//!   (GridWorld+0x498 origin, size of the render's material snapshot): cell→particle
//!   conversions (`FUN_007168c0`), material particles (`FUN_00716af0`) and particle effects
//!   (`FUN_00716ce0`).
//! - Cosmetic particles (+0x6a bit 1) collide with the render's material snapshot of the
//!   screen instead of the world, and land (or wrap) when they leave it (`FUN_00712390`).
//! - Material particle effects (`<ParticleEffect render_on_grid="1">`: sparkles and bubbles
//!   over magic liquids, acid, lava, poison, ...) are emitted from the on-screen surface
//!   cells the grid render job finds.
//! - Rain and snow (`GameEmitRainParticles`, vanilla `data/scripts/init.lua`) fall in a band
//!   around the camera.
//!
//! Inside the lockstep window the camera is the simulation camera (player 1), the snapshot
//! belongs to each client's own render, and the render runs outside the window. The patches
//! here make them per player: "on screen" means inside some player's view ([`views`]), the
//! snapshot lookup reads the world as the snapshot shows it, the emitters are found by
//! scanning the world between window exit and the next entry, and rain falls around every
//! player.

use crate::patches::{expect, in_window, rd, rel32, write_code, Exec, PatchError, GAME_GLOBAL_PTR};
use crate::views;
use std::sync::atomic::{AtomicBool, Ordering};

// --- view-bound particle creation ---------------------------------------------------------

/// `FUN_007168c0` (cell→particle; ECX = GridParticles, EDX = cell x, ESI = cell y):
/// `mov eax,[ecx+0x20]; sub edx,[eax+0x498]; sub esi,[eax+0x49c]; test edx,edx; js culled;
/// test esi,esi; js culled; mov eax,[ecx+0x24]; cmp edx,[eax]; jge culled; cmp esi,[eax+4];
/// jl visible`. The culled path puts the cell back into the grid (GridParticles+0x88).
const CELL_CULL: u32 = 0x0071_6943;
const CELL_CULL_ORIG: [u8; 35] = [
    0x8B, 0x41, 0x20, 0x2B, 0x90, 0x98, 0x04, 0x00, 0x00, 0x2B, 0xB0, 0x9C, 0x04, 0x00, 0x00, 0x85, 0xD2, 0x78, 0x10, 0x85,
    0xF6, 0x78, 0x0C, 0x8B, 0x41, 0x24, 0x3B, 0x10, 0x7D, 0x05, 0x3B, 0x70, 0x04, 0x7C, 0x20,
];
const CELL_VISIBLE: u32 = 0x0071_6986;
const CELL_CULLED: u32 = 0x0071_6966;
/// `FUN_00716af0` (material particle; EDI = GridParticles, ECX = x, ESI = y): the same test.
const MATERIAL_CULL: u32 = 0x0071_6bb3;
const MATERIAL_CULL_ORIG: [u8; 51] = [
    0x8B, 0x47, 0x20, 0x2B, 0x88, 0x98, 0x04, 0x00, 0x00, 0x2B, 0xB0, 0x9C, 0x04, 0x00, 0x00, 0x85, 0xC9, 0x0F, 0x88, 0xDA,
    0x00, 0x00, 0x00, 0x85, 0xF6, 0x0F, 0x88, 0xD2, 0x00, 0x00, 0x00, 0x8B, 0x47, 0x24, 0x3B, 0x08, 0x0F, 0x8D, 0xC7, 0x00,
    0x00, 0x00, 0x3B, 0x70, 0x04, 0x0F, 0x8D, 0xBE, 0x00, 0x00, 0x00,
];
const MATERIAL_VISIBLE: u32 = 0x0071_6be6;
const MATERIAL_CULLED: u32 = 0x0071_6ca4;
/// `FUN_00716ce0` (particle effect; EBX = GridParticles, ESI = float pos[2]): the same test
/// on `cvttss2si` of the position.
const EFFECT_CULL: u32 = 0x0071_6cf7;
const EFFECT_CULL_ORIG: [u8; 60] = [
    0xF3, 0x0F, 0x2C, 0x16, 0x8B, 0x43, 0x20, 0xF3, 0x0F, 0x2C, 0x4E, 0x04, 0x2B, 0x90, 0x98, 0x04, 0x00, 0x00, 0x2B, 0x88,
    0x9C, 0x04, 0x00, 0x00, 0x85, 0xD2, 0x0F, 0x88, 0xBA, 0x01, 0x00, 0x00, 0x85, 0xC9, 0x0F, 0x88, 0xB2, 0x01, 0x00, 0x00,
    0x8B, 0x43, 0x24, 0x3B, 0x10, 0x0F, 0x8D, 0xA7, 0x01, 0x00, 0x00, 0x3B, 0x48, 0x04, 0x0F, 0x8D, 0x9E, 0x01, 0x00, 0x00,
];
const EFFECT_VISIBLE: u32 = 0x0071_6d33;
const EFFECT_CULLED: u32 = 0x0071_6ed1;

/// Whether a view-bound particle at world pixel (x, y) is "on screen". Inside the window:
/// in some player's view (anywhere before the first window published the players).
/// Outside: vanilla's test against this client's camera rect and snapshot size.
extern "C" fn particle_on_screen(particles: u32, x: i32, y: i32) -> u32 {
    if in_window() {
        return (views::count() == 0 || views::in_any(x, y)) as u32;
    }
    unsafe {
        let grid_world = rd(particles + 0x20);
        let snapshot = rd(particles + 0x24);
        let dx = x.wrapping_sub(rd(grid_world + 0x498) as i32);
        let dy = y.wrapping_sub(rd(grid_world + 0x49c) as i32);
        (dx >= 0 && dy >= 0 && dx < rd(snapshot) as i32 && dy < rd(snapshot + 4) as i32) as u32
    }
}

/// Code for `at` replacing a cull test: `<prefix>; mov eax,particle_on_screen; call eax;
/// add esp,12; <suffix>; test eax,eax; jnz visible; jmp culled`, NOP-padded to `len`.
fn cull_code(at: u32, len: usize, prefix: &[u8], suffix: &[u8], visible: u32, culled: u32) -> Vec<u8> {
    let f = ((particle_on_screen as extern "C" fn(u32, i32, i32) -> u32) as u32).to_le_bytes();
    let mut c = prefix.to_vec();
    c.extend_from_slice(&[0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x0C]);
    c.extend_from_slice(suffix);
    c.extend_from_slice(&[0x85, 0xC0, 0x0F, 0x85]);
    let end = at + c.len() as u32 + 4;
    c.extend_from_slice(&rel32(end, visible));
    c.push(0xE9);
    let end = at + c.len() as u32 + 4;
    c.extend_from_slice(&rel32(end, culled));
    assert!(c.len() <= len);
    c.resize(len, 0x90);
    c
}

// --- cosmetic particle collisions ----------------------------------------------------------

/// `FUN_00712390` (particle collision step), for a cosmetic particle (+0x6a bit 1; EAX/EDX =
/// position relative to the snapshot origin GridWorld+0x498, EDI = snapshot {w, h, .., +0x18
/// material ids}): `test eax,eax; js out; cmp eax,[edi]; jge out; test edx,edx; js out; cmp
/// edx,[edi+4]; jl lookup`.
const SNAPSHOT_TEST: u32 = 0x0071_2547;
const SNAPSHOT_TEST_ORIG: [u8; 17] = [0x85, 0xC0, 0x78, 0x0D, 0x3B, 0x07, 0x7D, 0x09, 0x85, 0xD2, 0x78, 0x05, 0x3B, 0x57, 0x04, 0x7C, 0x2F];
/// In-snapshot path: `eax = snapshot[y][x]`, then the material-id handling at 0x007125c4.
const SNAPSHOT_LOOKUP: u32 = 0x0071_2587;
/// Outside the snapshot: wrap (+0x6b bit 2) or land (+0x6a |= 0x10).
const SNAPSHOT_OUTSIDE: u32 = 0x0071_2558;
/// The world-lookup path's continuation: reload XMM2-4 from the frame, then handle the
/// material id in EAX (-1 / 0 = nothing there).
const MATERIAL_ID_RELOAD: u32 = 0x0071_25b5;
/// [`cosmetic_cell`] results besides a material id.
const OUTSIDE: i32 = i32::MIN;
const VANILLA: i32 = i32::MIN + 1;

/// `cvttss2si` then floor: the pixel containing `v` (the snapshot index of a cosmetic
/// particle inside the snapshot).
pub(crate) fn floor_px(v: f32) -> i32 {
    if !(v > -2.1e9 && v < 2.1e9) {
        return i32::MIN;
    }
    let t = v as i32;
    if (t as f32) > v {
        t - 1
    } else {
        t
    }
}

/// thiscall vtable slot `slot` of a cell, no arguments.
unsafe fn cell_call(cell: u32, slot: u32) -> i32 {
    let f: extern "thiscall" fn(u32) -> i32 = std::mem::transmute(rd(rd(cell) + slot) as usize);
    f(cell)
}

/// The cell at world pixel (x, y), or 0 (also when its chunk is not loaded).
unsafe fn cell_at(chunk_table: u32, x: i32, y: i32) -> u32 {
    let idx = ((((y >> 9) + 256) & 511) * 512 + (((x >> 9) + 256) & 511)) as u32;
    let chunk = rd(chunk_table + 4 * idx);
    if chunk == 0 {
        return 0;
    }
    rd(rd(chunk) + 4 * ((((y & 511) as u32) << 9) | (x & 511) as u32))
}

/// The chunk table (GridWorld+0x500 ChunkMap, +8), or 0 without a world.
unsafe fn chunk_table() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    let world_data = if gg != 0 { rd(gg + 0x0c) } else { 0 };
    let grid_world = if world_data != 0 { rd(world_data + 0x44) } else { 0 };
    if grid_world != 0 {
        rd(grid_world + 0x508)
    } else {
        0
    }
}

/// What the grid render's material snapshot holds for a cell: its material id (vtbl+8), or
/// -1 for no cell and for cells the render does not draw (vtbl+0x10 == 4).
unsafe fn snapshot_value(cell: u32) -> i32 {
    if cell == 0 || cell_call(cell, 0x10) == 4 {
        -1
    } else {
        cell_call(cell, 0x08)
    }
}

/// The snapshot lookup of a cosmetic particle at (fx, fy). Inside the window: the world as
/// the snapshot would show it if the pixel is in some player's view ([`OUTSIDE`] if not; the
/// whole world before the first window published the players). Outside the window (no
/// particle steps run there): [`VANILLA`], the engine's own snapshot test.
extern "C" fn cosmetic_cell(fx: f32, fy: f32) -> i32 {
    if !in_window() {
        return VANILLA;
    }
    let (x, y) = (floor_px(fx), floor_px(fy));
    if views::count() > 0 && !views::in_any(x, y) {
        return OUTSIDE;
    }
    unsafe {
        let table = chunk_table();
        if table == 0 {
            return -1;
        }
        snapshot_value(cell_at(table, x, y))
    }
}

/// movups [esp+16*i], xmm_i (i = 0..8) after `sub esp,0x80`; and the restore.
fn xmm_save() -> Vec<u8> {
    let mut c = vec![0x81, 0xEC, 0x80, 0x00, 0x00, 0x00];
    for i in 0..8u8 {
        c.extend_from_slice(&[0x0F, 0x11, 0x44 | (i << 3), 0x24, i * 16]);
    }
    c
}
fn xmm_restore() -> Vec<u8> {
    let mut c = Vec::new();
    for i in 0..8u8 {
        c.extend_from_slice(&[0x0F, 0x10, 0x44 | (i << 3), 0x24, i * 16]);
    }
    c.extend_from_slice(&[0x81, 0xC4, 0x80, 0x00, 0x00, 0x00]);
    c
}

/// Append `jcc rel32` / `jmp rel32` to `code` (placed at `base`).
fn jump(code: &mut Vec<u8>, base: u32, op: &[u8], to: u32) {
    code.extend_from_slice(op);
    let end = base + code.len() as u32 + 4;
    code.extend_from_slice(&rel32(end, to));
}

/// The cosmetic-collision stub (jumped to from [`SNAPSHOT_TEST`]).
unsafe fn cosmetic_stub(ex: &mut Exec) -> u32 {
    // The stub is assembled for its final address: reserve, build, write.
    let base = ex.alloc_data(256);
    let f = ((cosmetic_cell as extern "C" fn(f32, f32) -> i32) as u32).to_le_bytes();
    let mut c = vec![0x50, 0x52, 0x51]; // push eax; push edx; push ecx
    c.extend(xmm_save());
    c.extend_from_slice(&[0xFF, 0x75, 0xD4, 0xFF, 0x75, 0xD0]); // push [ebp-0x2c] (y); push [ebp-0x30] (x)
    c.extend_from_slice(&[0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08]); // mov eax,f; call eax; add esp,8
    c.extend(xmm_restore());
    c.extend_from_slice(&[0x59, 0x5A]); // pop ecx; pop edx
    c.push(0x3D);
    c.extend_from_slice(&OUTSIDE.to_le_bytes()); // cmp eax,OUTSIDE
    c.extend_from_slice(&[0x74, 0x00]); // je outside
    let je_outside = c.len() - 1;
    c.push(0x3D);
    c.extend_from_slice(&VANILLA.to_le_bytes()); // cmp eax,VANILLA
    c.extend_from_slice(&[0x74, 0x00]); // je vanilla
    let je_vanilla = c.len() - 1;
    c.extend_from_slice(&[0x83, 0xC4, 0x04]); // add esp,4 (saved eax): eax = material id
    jump(&mut c, base, &[0xE9], MATERIAL_ID_RELOAD);
    c[je_outside] = (c.len() - je_outside - 1) as u8;
    c.push(0x58); // outside: pop eax
    jump(&mut c, base, &[0xE9], SNAPSHOT_OUTSIDE);
    c[je_vanilla] = (c.len() - je_vanilla - 1) as u8;
    c.push(0x58); // vanilla: pop eax; the original test
    c.extend_from_slice(&[0x85, 0xC0]);
    jump(&mut c, base, &[0x0F, 0x88], SNAPSHOT_OUTSIDE);
    c.extend_from_slice(&[0x3B, 0x07]);
    jump(&mut c, base, &[0x0F, 0x8D], SNAPSHOT_OUTSIDE);
    c.extend_from_slice(&[0x85, 0xD2]);
    jump(&mut c, base, &[0x0F, 0x88], SNAPSHOT_OUTSIDE);
    c.extend_from_slice(&[0x3B, 0x57, 0x04]);
    jump(&mut c, base, &[0x0F, 0x8C], SNAPSHOT_LOOKUP);
    jump(&mut c, base, &[0xE9], SNAPSHOT_OUTSIDE);
    assert!(c.len() <= 256);
    std::ptr::copy_nonoverlapping(c.as_ptr(), base as *mut u8, c.len());
    base
}

/// View-bound particle creation and cosmetic particle collisions follow every player's
/// view inside the window (see the module docs).
pub(crate) unsafe fn patch_particle_views(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CELL_CULL, &CELL_CULL_ORIG)?;
    expect(MATERIAL_CULL, &MATERIAL_CULL_ORIG)?;
    expect(EFFECT_CULL, &EFFECT_CULL_ORIG)?;
    expect(SNAPSHOT_TEST, &SNAPSHOT_TEST_ORIG)?;
    // push ecx (kept: the visible path's calls and the culled path use it); push esi (y);
    // push edx (x); push ecx (GridParticles) ... pop ecx
    let cell = cull_code(CELL_CULL, CELL_CULL_ORIG.len(), &[0x51, 0x56, 0x52, 0x51], &[0x59], CELL_VISIBLE, CELL_CULLED);
    // push esi (y); push ecx (x); push edi (GridParticles)
    let material = cull_code(MATERIAL_CULL, MATERIAL_CULL_ORIG.len(), &[0x56, 0x51, 0x57], &[], MATERIAL_VISIBLE, MATERIAL_CULLED);
    // cvttss2si edx,[esi]; cvttss2si ecx,[esi+4]; push ecx (y); push edx (x); push ebx
    let effect = cull_code(
        EFFECT_CULL,
        EFFECT_CULL_ORIG.len(),
        &[0xF3, 0x0F, 0x2C, 0x16, 0xF3, 0x0F, 0x2C, 0x4E, 0x04, 0x51, 0x52, 0x53],
        &[],
        EFFECT_VISIBLE,
        EFFECT_CULLED,
    );
    let stub = cosmetic_stub(ex);
    let mut snapshot = vec![0xE9];
    snapshot.extend_from_slice(&rel32(SNAPSHOT_TEST + 5, stub));
    snapshot.resize(SNAPSHOT_TEST_ORIG.len(), 0x90);
    write_code(CELL_CULL, &cell)?;
    write_code(MATERIAL_CULL, &material)?;
    write_code(EFFECT_CULL, &effect)?;
    write_code(SNAPSHOT_TEST, &snapshot)
}

// --- material particle effects (cell emitters) ---------------------------------------------

/// Grid render job (FUN_00724580): `jz 0x00724afc` skipping a cell whose material has no
/// particle effect (`cmp dword [material+0x28c],0`); otherwise the cell is queued as an
/// emitter (DAT_012080d8) for FUN_00724480, which emits after the render.
const RENDER_EMITTER_JZ: u32 = 0x0072_4977;
/// FUN_007171d0(this = *(GameGlobal+0x14), const float pos[2], ParticleEffect*), thiscall,
/// RET 8: draws the count (global LCG), then per particle the probability (particle RNG)
/// and the velocity (global LCG), and creates a view-bound particle (FUN_00716ce0).
const EMIT_EFFECT_FN: u32 = 0x0071_71d0;
/// CellData+0x28c: the material's `render_on_grid` ParticleEffect (0 = none).
const MATERIAL_EFFECT: u32 = 0x28c;

static EMITTERS_ON: AtomicBool = AtomicBool::new(false);

/// The graphics device's buffer swap (`FUN_00dd9450`, fastcall `this`: SDL_GL_SwapWindow of
/// the device's window), a vtable slot.
const SWAP_SLOT: u32 = 0x0105_3238;
const SWAP_FN: u32 = 0x00dd_9450;

/// Material particle effects come from the surface cells of every player's view, scanned
/// from the world between window exit and the next entry, instead of from the grid render
/// (this client's view, outside the window). The render no longer queues emitters.
pub(crate) unsafe fn patch_cell_emitters(ex: &mut Exec) -> Result<(), PatchError> {
    expect(RENDER_EMITTER_JZ - 14, &[0x83, 0xB8, 0x8C, 0x02, 0x00, 0x00, 0x00])?;
    expect(RENDER_EMITTER_JZ, &[0x0F, 0x84, 0x7F, 0x01, 0x00, 0x00])?;
    expect(SWAP_SLOT, &SWAP_FN.to_le_bytes())?;
    // jz rel32 -> jmp rel32; nop
    let rel = rel32(RENDER_EMITTER_JZ + 5, RENDER_EMITTER_JZ + 6 + 0x17f);
    write_code(RENDER_EMITTER_JZ, &[0xE9, rel[0], rel[1], rel[2], rel[3], 0x90])?;
    // The swap starts the scan: xmm save; pushad; call swap_start; popad; xmm restore; jmp swap.
    let base = ex.alloc_data(160);
    let f = ((swap_start as extern "C" fn()) as u32).to_le_bytes();
    let mut c = xmm_save();
    c.extend_from_slice(&[0x60, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x61]);
    c.extend(xmm_restore());
    jump(&mut c, base, &[0xE9], SWAP_FN);
    assert!(c.len() <= 160);
    std::ptr::copy_nonoverlapping(c.as_ptr(), base as *mut u8, c.len());
    write_code(SWAP_SLOT, &base.to_le_bytes())?;
    EMITTERS_ON.store(true, Ordering::Relaxed);
    Ok(())
}

/// Set at window exit, taken by the next buffer swap ([`swap_start`]).
static SCAN_ARMED: AtomicBool = AtomicBool::new(false);

/// Window exit: the next buffer swap starts the emitter scan ([`scan_cell_emitters`]). The
/// swap is when this client's pool workers are idle (the render tiles are joined), so the
/// scan does not compete with them; it must finish by the next window entry either way.
pub fn arm_cell_emitters() {
    SCAN_ARMED.store(EMITTERS_ON.load(Ordering::Relaxed), Ordering::Relaxed);
}

extern "C" fn swap_start() {
    if SCAN_ARMED.swap(false, Ordering::Relaxed) {
        // SAFETY: game thread, between window exit and the next entry.
        unsafe { scan_cell_emitters() };
    }
}

/// Surface cells with a material particle effect in the union of every player's view, in
/// row order: (x, y, ParticleEffect*). The grid render's rule: the cell is drawn
/// (vtbl+0x10 != 4), its material has an effect, and the snapshot pixel above it is empty
/// (no cell, an undrawn one, or material id <= 0).
unsafe fn find_emitters(table: u32, rects: &[[i32; 4]], out: &mut Vec<(i32, i32, u32)>) {
    views::union_rows(rects, |y, spans| {
        let cy = (((y >> 9) + 256) & 511) as u32;
        for &(x0, x1) in spans {
            let mut x = x0;
            while x < x1 {
                // The part of the span inside this chunk.
                let end = x1.min((x & !511) + 512);
                let chunk = rd(table + 4 * (cy * 512 + (((x >> 9) + 256) & 511) as u32));
                if chunk != 0 {
                    let row = (rd(chunk) + 4 * (((y & 511) as u32) << 9)) as *const u32;
                    let (i0, i1) = ((x & 511) as usize, ((end - 1) & 511) as usize + 1);
                    for i in i0..i1 {
                        // Cells are scattered heap objects: fetch the ones ahead while this
                        // one is tested (as `world::hash_cells`).
                        const AHEAD: usize = 24;
                        if i + AHEAD < i1 {
                            let next = *row.add(i + AHEAD);
                            if next != 0 {
                                crate::world::prefetch((next + 0x14) as *const u8);
                            }
                        }
                        let cell = *row.add(i);
                        if cell == 0 {
                            continue;
                        }
                        let effect = rd(rd(cell + 0x14) + MATERIAL_EFFECT);
                        if effect == 0 || cell_call(cell, 0x10) == 4 {
                            continue;
                        }
                        let px = (x & !511) + i as i32;
                        if snapshot_value(cell_at(table, px, y - 1)) <= 0 {
                            out.push((px, y, effect));
                        }
                    }
                }
                x = end;
            }
        }
    });
}

/// The emitter scan runs on a worker thread between window exit ([`scan_cell_emitters`])
/// and the next window entry ([`emit_cell_particles`]), while this client renders. Cells
/// only change inside the simulation window (see [`crate::report`]), so the scan sees
/// exactly the world the entry would have seen, and the views it uses are the ones the
/// entry would use (they are republished only after the entry).
struct Scan {
    /// A scan is on the worker.
    running: bool,
    /// `found` holds the scan started at the last window exit.
    ready: bool,
    found: Vec<(i32, i32, u32)>,
}

type ScanJob = (u32, Vec<[i32; 4]>, Vec<(i32, i32, u32)>);

static SCAN: std::sync::Mutex<Scan> = std::sync::Mutex::new(Scan { running: false, ready: false, found: Vec::new() });
static SCAN_DONE: std::sync::Condvar = std::sync::Condvar::new();
static SCAN_QUEUE: std::sync::Mutex<Option<std::sync::mpsc::Sender<ScanJob>>> = std::sync::Mutex::new(None);

fn scan_state() -> std::sync::MutexGuard<'static, Scan> {
    SCAN.lock().unwrap_or_else(|p| p.into_inner())
}

/// Wait for the scan started since the last window exit.
fn join_scan() -> std::sync::MutexGuard<'static, Scan> {
    let mut state = scan_state();
    while state.running {
        state = SCAN_DONE.wait(state).unwrap_or_else(|p| p.into_inner());
    }
    state
}

/// Wait for a running emitter scan (shutdown: the world may be torn down next).
pub fn join_cell_emitters() {
    drop(join_scan());
}

/// Start scanning for the next window entry's emitters on the worker (buffer swap after a
/// window exit; without a worker the scan runs here).
///
/// # Safety
/// Game thread, between window exit and the next entry.
unsafe fn scan_cell_emitters() {
    if !EMITTERS_ON.load(Ordering::Relaxed) || views::count() == 0 {
        return;
    }
    let table = chunk_table();
    if table == 0 {
        return;
    }
    let mut state = join_scan();
    let mut queue = SCAN_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<ScanJob>();
        let worker = std::thread::Builder::new().name("noita-online-emitters".into()).spawn(move || {
            for (table, rects, mut found) in rx {
                found.clear();
                // SAFETY: the world does not change before the next window entry joins this.
                unsafe { find_emitters(table, &rects, &mut found) };
                let mut state = scan_state();
                state.found = found;
                state.running = false;
                SCAN_DONE.notify_all();
            }
        });
        if worker.is_ok() {
            *queue = Some(tx);
        }
    }
    state.ready = true;
    let job = (table, views::rects(), std::mem::take(&mut state.found));
    let sent = match queue.as_ref() {
        Some(tx) => tx.send(job).map_err(|e| e.0),
        None => Err(job),
    };
    match sent {
        Ok(()) => state.running = true,
        Err((table, rects, mut found)) => {
            found.clear();
            find_emitters(table, &rects, &mut found);
            state.found = found;
        }
    }
}

/// Emit this frame's material particle effects (window entry, simulation RNG bank loaded):
/// what the grid render of every player's view would have queued at the end of the last
/// frame, in a fixed order.
///
/// # Safety
/// Game thread, inside the window, before the world update.
pub unsafe fn emit_cell_particles() {
    // A window exit not followed by a swap: the scan runs below instead.
    SCAN_ARMED.store(false, Ordering::Relaxed);
    let mut state = join_scan();
    let ready = std::mem::replace(&mut state.ready, false);
    if !EMITTERS_ON.load(Ordering::Relaxed) || views::count() == 0 {
        return;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    let table = chunk_table();
    if gg == 0 || table == 0 || rd(gg + 0x14) == 0 {
        return;
    }
    if !ready {
        // No scan since the last window entry (the first window): scan here.
        state.found.clear();
        find_emitters(table, &views::rects(), &mut state.found);
    }
    let emit: extern "thiscall" fn(u32, *const f32, u32) = std::mem::transmute(EMIT_EFFECT_FN as usize);
    let this = rd(gg + 0x14);
    for &(x, y, effect) in state.found.iter() {
        // FUN_00724480: pos = (x - 0.0f, y - 1.0f)
        let pos = [x as f32 - 0.0, y as f32 - 1.0];
        emit(this, pos.as_ptr(), effect);
    }
}

// --- rain and snow ------------------------------------------------------------------------

/// GameEmitRainParticles (Lua binding FUN_007c6b90): `test edi,edi; jz done; nop; loop: lea
/// ecx,[esp+0x14]; call FUN_00c0e9f0; dec edi; jnz loop` (EDI = num_particles; [esp+0x14] =
/// {width_outside_camera, material, velocity min/max, gravity, flags}).
const RAIN_LOOP: u32 = 0x007c_6f4b;
const RAIN_LOOP_ORIG: [u8; 17] = [0x85, 0xFF, 0x74, 0x0D, 0x90, 0x8D, 0x4C, 0x24, 0x14, 0xE8, 0x97, 0x7A, 0x44, 0x00, 0x4F, 0x75, 0xF4];
/// FUN_00c0e9f0(params) (fastcall): one rain particle, if the camera's view top
/// (GridWorld+0x49c) is above the rain altitude: x random in [view x - width_outside, view x
/// + view width + width_outside] (GridWorld+0x498/+0x4a0), y random in the 512 px above the
/// view top, global LCG draws; created as a world particle (FUN_00716af0).
const RAIN_PARTICLE_FN: u32 = 0x00c0_e9f0;

/// The rain loop calls [`rain_emit`] instead.
pub(crate) unsafe fn patch_rain(_: &mut Exec) -> Result<(), PatchError> {
    expect(RAIN_LOOP, &RAIN_LOOP_ORIG)?;
    let f = ((rain_emit as extern "C" fn(u32, i32)) as u32).to_le_bytes();
    // lea ecx,[esp+0x14]; push edi; push ecx; mov eax,rain_emit; call eax; add esp,8; nop
    // (falls through to the loop's exit)
    let code = [0x8D, 0x4C, 0x24, 0x14, 0x57, 0x51, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x90];
    write_code(RAIN_LOOP, &code)
}

/// Emit `n` rain particles around every player's camera (vanilla: around the one camera).
/// A player's band is skipped when an earlier player's emitted band already covers its view
/// horizontally from the same height or above, so overlapping views get vanilla's density.
/// Outside the window or with fewer than two views: vanilla, around the engine camera.
extern "C" fn rain_emit(params: u32, n: i32) {
    unsafe {
        let emit: extern "fastcall" fn(u32) = std::mem::transmute(RAIN_PARTICLE_FN as usize);
        let count = views::count();
        let gg = rd(GAME_GLOBAL_PTR);
        let cam = if gg != 0 { rd(gg + 0x0c) } else { 0 };
        let grid_world = if cam != 0 { rd(cam + 0x44) } else { 0 };
        if !in_window() || count < 2 || grid_world == 0 {
            for _ in 0..n {
                emit(params);
            }
            return;
        }
        // The engine camera (rect x, y, w, h) and its int view rect origin; each player's view
        // is the same rect moved by its centre's offset from the camera centre.
        let rect = |i: u32| *((cam + 4 * i) as *const f32);
        let (cx, cy) = (rect(0) + rect(2) * 0.5, rect(1) + rect(3) * 0.5);
        let (ox, oy) = (rd(grid_world + 0x498), rd(grid_world + 0x49c));
        let band = *(params as *const f32);
        let mut emitted = [false; views::MAX_VIEWS];
        for k in 0..count {
            let (kx, ky) = views::centre(k);
            let covered = (0..k).any(|j| {
                let (jx, jy) = views::centre(j);
                emitted[j] && (kx - jx).abs() <= band && jy <= ky
            });
            if covered {
                continue;
            }
            emitted[k] = true;
            *((grid_world + 0x498) as *mut i32) = (ox as i32).wrapping_add(floor_px(kx - cx + 0.5));
            *((grid_world + 0x49c) as *mut i32) = (oy as i32).wrapping_add(floor_px(ky - cy + 0.5));
            for _ in 0..n {
                emit(params);
            }
        }
        *((grid_world + 0x498) as *mut u32) = ox;
        *((grid_world + 0x49c) as *mut u32) = oy;
    }
}
