//! The falling-sand tile budget for every player's view.
//!
//! The grid update (`FUN_00726340`, inside the simulation window) simulates at most
//! `GRID_MAX_UPDATES_PER_FRAME` (128) active 64×64 tiles per frame. With more active tiles
//! it sorts them by distance to the centre of `GridWorld::GetBorders` (the camera rect
//! snapped to 64 px, grown by 64 px on each side), keeps the nearest 128 and defers the rest
//! (a shuffled list the update refills from whenever fewer tiles are active than the cap).
//! Inside the window that camera is the simulation camera (first live player), so with
//! several players every other player's sand, water and fire starved whenever the world was
//! busy, in favour of player 1's surroundings.
//!
//! Each player now keeps vanilla's budget around its own view: the cap is 128 per view, every
//! active tile belongs to its nearest view, each view's tiles are ordered by vanilla's
//! comparator around that view, and the views are interleaved rank by rank (slot order), so
//! the kept tiles are every view's nearest `min(own tiles, 128)` (budget a view does not use
//! goes to the other views' next-nearest tiles). With one view (or none) the update is
//! untouched.

use crate::patches::{expect, rel32, write_code, Exec, PatchError};
use crate::views;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};

/// FUN_00726340, after the cap is known (ESI = `[ebp-0x64]` = cap; EDI = active list
/// {count, tile array, ..., +0x30 deferred count, +0x34 deferred array}):
/// `mov ecx,[edi]; cmp ecx,esi; jle refill`. Above the cap: sort (below), defer tiles
/// [cap, count), shuffle the deferred list (CRT `rand`), count = cap; at or below it: move
/// up to cap - count deferred tiles back. `[ebp-0x64]` is reloaded as the cap at 0x00726d5c
/// and 0x00726dc0 and not read again before the slot is reused (0x0072734d).
const CAP_TEST: u32 = 0x0072_6d05;
const CAP_TEST_ORIG: [u8; 10] = [0x8B, 0x0F, 0x3B, 0xCE, 0x0F, 0x8E, 0xD2, 0x00, 0x00, 0x00];
const CAP_REFILL: u32 = 0x0072_6de1;
const CAP_OVER: u32 = 0x0072_6d0f;

/// `call FUN_00726340 → FUN_0095d050`: MSVC `std::sort` of the active tiles (ECX = first,
/// EDX = last; stack, caller-popped: ideal = count, x, y = GetBorders centre `x + w/2`,
/// `y + h/2` as ints). Comparator `FUN_00725e60`: tile centre from its rect (vtable+0x5c,
/// `FUN_007222d0` = tile+0x470 {x, y, w, h}): `x + w/2`, `y + h/2` (int, truncating);
/// less = `dy² + dx²` (wrapping i32) smaller, signed.
const TILE_SORT_CALL: u32 = 0x0072_6d57;
const TILE_SORT_FN: u32 = 0x0095_d050;
/// push ebp; mov ebp,esp; sub esp,0x10; push ebx; push esi
const TILE_SORT_PROLOGUE: [u8; 8] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x10, 0x53, 0x56];
/// cdecl (first, last, ideal, x, y) thunk to FUN_0095d050.
static VANILLA_SORT: AtomicU32 = AtomicU32::new(0);

/// The cap for this frame: `cap` per view (vanilla's with one view or none).
extern "C" fn grid_cap(cap: i32) -> i32 {
    let n = views::count();
    if n <= 1 {
        cap
    } else {
        cap.saturating_mul(n as i32)
    }
}

/// Tile centre as FUN_00725e60 computes it.
unsafe fn tile_centre(tile: u32) -> (i32, i32) {
    let mut out = [0i32; 4];
    let get: extern "thiscall" fn(u32, *mut [i32; 4]) -> *const [i32; 4] =
        std::mem::transmute(*((*(tile as *const u32) + 0x5c) as *const u32) as usize);
    let r = *get(tile, &mut out);
    (r[0].wrapping_add(r[2] / 2), r[1].wrapping_add(r[3] / 2))
}

/// FUN_00725e60's distance key: `dy² + dx²`, wrapping i32.
fn dist2(t: (i32, i32), c: (i32, i32)) -> i32 {
    let (dx, dy) = (t.0.wrapping_sub(c.0), t.1.wrapping_sub(c.1));
    dy.wrapping_mul(dy).wrapping_add(dx.wrapping_mul(dx))
}

#[derive(Default)]
struct Scratch {
    /// Per tile (array order): its view and its key around that view.
    view: Vec<u32>,
    key: Vec<i32>,
    /// Tile indices grouped by view (array order within a group), then sorted per group.
    group: Vec<u32>,
    starts: Vec<usize>,
    out: Vec<u32>,
}

thread_local! {
    /// Reused between frames (the grid update runs on the game thread).
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// The active-tile sort. One view or none: the engine's sort (vanilla). More: each tile goes
/// to its nearest view centre (lowest slot on ties), each view's tiles are sorted with MSVC's
/// `std::sort` (as the engine would sort that subsequence) by vanilla's key around the view,
/// and the result is interleaved rank-major in slot order.
///
/// View centres: vanilla sorts around the GetBorders centre (x, y), i.e. the simulation
/// camera = view 0 shifted by the borders' 64 px snapping. Every view centre gets the same
/// shift, so view 0's centre is exactly vanilla's.
extern "C" fn grid_sort(first: u32, last: u32, ideal: i32, x: i32, y: i32) {
    unsafe {
        let n = views::count();
        if n <= 1 {
            let sort: extern "C" fn(u32, u32, i32, i32, i32) = std::mem::transmute(VANILLA_SORT.load(Ordering::Relaxed) as usize);
            sort(first, last, ideal, x, y);
            return;
        }
        if last <= first {
            return;
        }
        let tiles = std::slice::from_raw_parts_mut(first as *mut u32, ((last - first) / 4) as usize);
        let px = |i: usize| {
            let (vx, vy) = views::centre(i);
            (vx.floor() as i32, vy.floor() as i32)
        };
        let v0 = px(0);
        let (sx, sy) = (x.wrapping_sub(v0.0), y.wrapping_sub(v0.1));
        let mut centres = [(0i32, 0i32); views::MAX_VIEWS];
        for (i, c) in centres.iter_mut().enumerate().take(n) {
            let v = px(i);
            *c = (v.0.wrapping_add(sx), v.1.wrapping_add(sy));
        }
        let centres = &centres[..n];
        SCRATCH.with(|s| {
            let Scratch { view, key, group, starts, out } = &mut *s.borrow_mut();
            view.clear();
            key.clear();
            for &t in tiles.iter() {
                let tc = tile_centre(t);
                let (mut best, mut best_d) = (0u32, dist2(tc, centres[0]));
                for (i, &c) in centres.iter().enumerate().skip(1) {
                    let d = dist2(tc, c);
                    if d < best_d {
                        (best, best_d) = (i as u32, d);
                    }
                }
                view.push(best);
                key.push(best_d);
            }
            group.clear();
            starts.clear();
            for i in 0..n as u32 {
                starts.push(group.len());
                group.extend((0..tiles.len() as u32).filter(|&k| view[k as usize] == i));
            }
            starts.push(group.len());
            let key = &*key;
            for i in 0..n {
                let (a, b) = (starts[i], starts[i + 1]);
                crate::cambound::msvc_sort(group, a, b, (b - a) as i32, &|p: u32, q: u32| key[p as usize] < key[q as usize]);
            }
            out.clear();
            for rank in 0.. {
                let before = out.len();
                for i in 0..n {
                    if starts[i] + rank < starts[i + 1] {
                        out.push(tiles[group[starts[i] + rank] as usize]);
                    }
                }
                if out.len() == before {
                    break;
                }
            }
            tiles.copy_from_slice(out);
        });
    }
}

/// Put `code` into the stub area, then point the rel32 fields at `code[at..at + 4]` to their
/// targets.
unsafe fn put_fixed(ex: &mut Exec, code: &[u8], rels: &[(usize, u32)]) -> Result<u32, PatchError> {
    let base = ex.put(code);
    for &(at, target) in rels {
        let end = base + at as u32 + 4;
        write_code(end - 4, &rel32(end, target))?;
    }
    Ok(base)
}

/// The grid update's tile budget per view (module docs).
pub(crate) unsafe fn patch_grid_cap(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CAP_TEST, &CAP_TEST_ORIG)?;
    let sort_rel = rel32(TILE_SORT_CALL + 5, TILE_SORT_FN);
    expect(TILE_SORT_CALL, &[0xE8, sort_rel[0], sort_rel[1], sort_rel[2], sort_rel[3]])?;
    expect(TILE_SORT_FN, &TILE_SORT_PROLOGUE)?;

    // Engine sort thunk: mov ecx,[esp+4]; mov edx,[esp+8]; push [esp+0x14] x3 (y, x, ideal);
    // call FUN_0095d050; add esp,0xc; ret
    let thunk = put_fixed(
        ex,
        &[
            0x8B, 0x4C, 0x24, 0x04, 0x8B, 0x54, 0x24, 0x08, 0xFF, 0x74, 0x24, 0x14, 0xFF, 0x74, 0x24, 0x14, 0xFF, 0x74, 0x24, 0x14,
            0xE8, 0, 0, 0, 0, 0x83, 0xC4, 0x0C, 0xC3,
        ],
        &[(21, TILE_SORT_FN)],
    )?;
    VANILLA_SORT.store(thunk, Ordering::Relaxed);

    // Cap test: jmp stub; NOP x5. Stub (EAX/ECX/EDX are dead here, no XMM register is live):
    // push esi; mov eax,grid_cap; call eax; add esp,4; mov esi,eax; mov [ebp-0x64],eax;
    // mov ecx,[edi]; cmp ecx,esi; jle refill; jmp over
    let f = ((grid_cap as extern "C" fn(i32) -> i32) as u32).to_le_bytes();
    let stub = put_fixed(
        ex,
        &[
            0x56, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x8B, 0xF0, 0x89, 0x45, 0x9C, 0x8B, 0x0F, 0x3B, 0xCE,
            0x0F, 0x8E, 0, 0, 0, 0, 0xE9, 0, 0, 0, 0,
        ],
        &[(22, CAP_REFILL), (27, CAP_OVER)],
    )?;
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(CAP_TEST + 5, stub));
    jmp.resize(CAP_TEST_ORIG.len(), 0x90);
    write_code(CAP_TEST, &jmp)?;

    // Sort: call stub; stub: push [esp+0xc] x3 (y, x, ideal); push edx; push ecx;
    // mov eax,grid_sort; call eax; add esp,0x14; ret
    let f = ((grid_sort as extern "C" fn(u32, u32, i32, i32, i32)) as u32).to_le_bytes();
    let stub = ex.put(&[
        0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x52, 0x51, 0xB8, f[0], f[1], f[2], f[3], 0xFF,
        0xD0, 0x83, 0xC4, 0x14, 0xC3,
    ]);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(TILE_SORT_CALL + 5, stub));
    write_code(TILE_SORT_CALL, &call)
}
