//! CameraBoundSystem (vtable 0x0102be30) for every player's view.
//!
//! Vanilla ties `CameraBoundComponent` entities (most enemies: `max_count` 20-30 per entity
//! file; ~60 entity files with a 500-2000 px `distance`: tanks, worms, fish, lanterns, ...)
//! to its one camera:
//! - Per entity (`FUN_00a5bd10`, the system's component update): outside the pixel AABB of
//!   the loaded chunks, or farther than `distance` from the camera centre, the entity is
//!   frozen (a record `{file, x, y, (distance - distance_border)^2, max_count}` in a map
//!   keyed by its 512 px cell, `FUN_00a58fe0`) and killed.
//! - On component add (`FUN_00a5bbe0`, vtable+0x34): the entity joins its entity file's
//!   vector (`FUN_00a58f20`); above `max_count` that vector is sorted by distance to the
//!   camera centre and all but the nearest `max_count` are killed (frozen first if
//!   `freeze_on_max_count_kill`).
//! - Every system update (`FUN_00a5be10`, vtable+0x28): the records of the 5x5 cells
//!   (±1024 px) around the camera centre respawn (`FUN_00a590f0`) when they are within
//!   their `distance - distance_border` of the camera centre, off screen (beyond
//!   0.6 * max(camera w, h)), inside the loaded AABB and below their file's `max_count`.
//! - After a world load / player spawn (`FUN_00a5c730`, DeathMatch::Update before the
//!   simulation window): the records of the 3x3 cells around the camera respawn at once,
//!   on screen too.
//!
//! The system update and the per-entity test run in the entity-system loop (`FUN_00db6a10`,
//! inside the simulation window), and so do nearly all component adds (chunk loads, spawns,
//! Lua). There the camera is the simulation camera (first live player), so every other
//! player's surroundings were frozen, culled and repopulated as if they stood at player 1.
//! The patches here put each player's camera back: distances are to the nearest view centre
//! ([`views::nearest`]), so "near the camera" means near some player and "off screen" means
//! off every player's screen; the max_count sort keeps the entities nearest to some player;
//! the respawn grid runs around every view centre (each cell once). `max_count` stays per
//! entity file over the whole world, as in vanilla (it counts loaded entities, not entities
//! on screen). The load/spawn respawn runs outside the window and is skipped once views
//! exist ([`start_respawn`]). Without views (before the first window) everything is vanilla.
//!
//! With two or more views the loaded world is a set of islands (one per player) whose AABB
//! spans unloaded holes, so the AABB test no longer means "loaded": entities in an unloaded
//! chunk are frozen as if outside the world, and records never respawn into one.

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError, GAME_GLOBAL_PTR};
use crate::views;
use std::sync::atomic::{AtomicU32, Ordering};

// --- per-entity distance kill: FUN_00a5bd10 --------------------------------------------------

/// FUN_00a5bd10(Entity*, CameraBoundComponent*) (stdcall; ESI/EDI saved, EDI = component),
/// after the `enabled` (+0x48) test: `call FUN_00439bb0` .. `comiss xmm1,xmm0; jbe ret`.
/// Loads the ChunkMap (GridWorld vtable+0xc), tests the entity position (Entity+0x70/+0x74)
/// against its loaded-pixel AABB (+0x20 x0, +0x24 y0, +0x28 x1, +0x2c y1; outside → freeze
/// and kill at 0x00a5bdf5), then compares dist^2 to the camera centre (GG+0xc rect: x + w/2,
/// y + h/2) with `distance`^2 (component+0x4c; farther → 0x00a5bdef).
const ENTITY_TEST: u32 = 0x00a5_bd25;
const ENTITY_TEST_ORIG: [u8; 202] = [
    0xE8, 0x86, 0xDE, 0x9D, 0xFF, 0x8B, 0x40, 0x0C, 0x8B, 0x48, 0x44, 0x8B, 0x01, 0x8B, 0x40, 0x0C, 0xFF, 0xD0, 0x8B, 0x75,
    0x08, 0xF3, 0x0F, 0x6F, 0x48, 0x20, 0x66, 0x0F, 0x6F, 0xC1, 0x66, 0x0F, 0x73, 0xD8, 0x08, 0x66, 0x0F, 0x7E, 0xC0, 0x66,
    0x0F, 0x6F, 0xC1, 0x66, 0x0F, 0x73, 0xD8, 0x0C, 0x66, 0x0F, 0x6E, 0xD8, 0x66, 0x0F, 0x7E, 0xC0, 0x0F, 0x5B, 0xDB, 0x66,
    0x0F, 0x6E, 0xD0, 0x66, 0x0F, 0x7E, 0xC8, 0x66, 0x0F, 0x73, 0xD9, 0x04, 0x0F, 0x5B, 0xD2, 0x66, 0x0F, 0x6E, 0xC0, 0x66,
    0x0F, 0x7E, 0xC8, 0xF3, 0x0F, 0x10, 0x4E, 0x70, 0x0F, 0x5B, 0xC0, 0x66, 0x0F, 0x6E, 0xE0, 0x0F, 0x2F, 0xC8, 0x0F, 0x5B,
    0xE4, 0x72, 0x69, 0x0F, 0x2F, 0xD9, 0x76, 0x64, 0xF3, 0x0F, 0x10, 0x46, 0x74, 0x0F, 0x2F, 0xC4, 0x72, 0x5A, 0x0F, 0x2F,
    0xD0, 0x76, 0x55, 0xE8, 0x0B, 0xDE, 0x9D, 0xFF, 0xF3, 0x0F, 0x10, 0x47, 0x4C, 0xF3, 0x0F, 0x59, 0xC0, 0x8B, 0x40, 0x0C,
    0xF3, 0x0F, 0x10, 0x50, 0x08, 0xF3, 0x0F, 0x59, 0x15, 0x1C, 0x36, 0x05, 0x01, 0xF3, 0x0F, 0x10, 0x48, 0x0C, 0xF3, 0x0F,
    0x59, 0x0D, 0x1C, 0x36, 0x05, 0x01, 0xF3, 0x0F, 0x58, 0x10, 0xF3, 0x0F, 0x58, 0x48, 0x04, 0xF3, 0x0F, 0x5C, 0x56, 0x70,
    0xF3, 0x0F, 0x5C, 0x4E, 0x74, 0xF3, 0x0F, 0x59, 0xD2, 0xF3, 0x0F, 0x59, 0xC9, 0xF3, 0x0F, 0x58, 0xCA, 0x0F, 0x2F, 0xC8,
    0x76, 0x16,
];
/// `cmp byte [edi+0x58],0` (freeze_on_distance_kill), then freeze and/or kill.
const ENTITY_FAR: u32 = 0x00a5_bdef;
/// Freeze (FUN_00a58fe0) and kill (FUN_0044df60), ECX = ESI = entity, EDX = EDI = component.
const ENTITY_OUTSIDE: u32 = 0x00a5_bdf5;
/// `pop edi; pop esi; ...; ret 8`: the entity stays.
const ENTITY_KEEP: u32 = 0x00a5_be05;

const KEEP: u32 = 0;
const FAR: u32 = 1;
const OUTSIDE: u32 = 2;

/// Entity position (Entity+0x70, +0x74).
unsafe fn entity_pos(entity: u32) -> (f32, f32) {
    (*((entity + 0x70) as *const f32), *((entity + 0x74) as *const f32))
}

/// The camera/world object (GG+0xc): float rect {x, y, w, h} at +0, GridWorld* at +0x44.
/// Every caller runs while the world exists (GameGlobal created).
unsafe fn world_data() -> u32 {
    rd(rd(GAME_GLOBAL_PTR) + 0x0c)
}

/// Vanilla's camera centre: `w * 0.5 + x`, `h * 0.5 + y` (that operation order).
unsafe fn camera_centre() -> (f32, f32) {
    let r = world_data() as *const f32;
    (*r.add(2) * 0.5 + *r, *r.add(3) * 0.5 + *r.add(1))
}

/// The ChunkMap, fetched as CameraBound does (GridWorld vtable+0xc): chunk table at +8,
/// pixel AABB of the loaded chunks at +0x20.
unsafe fn chunk_map() -> u32 {
    let grid_world = rd(world_data() + 0x44);
    let get: extern "thiscall" fn(u32) -> u32 = std::mem::transmute(rd(rd(grid_world) + 0x0c) as usize);
    get(grid_world)
}

/// Whether the chunk containing world position (x, y) is loaded (FUN_0089c1b0's lookup on
/// the floored pixel).
unsafe fn chunk_loaded(chunk_map: u32, x: f32, y: f32) -> bool {
    let (px, py) = (x.floor() as i32, y.floor() as i32);
    let idx = ((((py >> 9) - 0x100) & 0x1ff) << 9) | (((px >> 9) - 0x100) & 0x1ff);
    rd(rd(chunk_map + 8) + 4 * idx as u32) != 0
}

/// The loaded world in the islands case (two or more views): the chunk itself must be loaded.
unsafe fn in_hole(chunk_map: u32, x: f32, y: f32) -> bool {
    views::count() >= 2 && !chunk_loaded(chunk_map, x, y)
}

/// Replaces the AABB and distance tests of FUN_00a5bd10 in place: `mov esi,[ebp+8]; push
/// edi; push esi; mov eax,entity_verdict; call eax; add esp,8; test eax,eax; jz keep; dec
/// eax; jz far; jmp outside`, NOP-padded to the original 202 bytes.
fn entity_test_code() -> Vec<u8> {
    let f = ((entity_verdict as extern "C" fn(u32, u32) -> u32) as u32).to_le_bytes();
    let mut c = vec![0x8B, 0x75, 0x08, 0x57, 0x56, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x08, 0x85, 0xC0];
    for (prefix, target) in [(&[0x0F, 0x84][..], ENTITY_KEEP), (&[0x48, 0x0F, 0x84][..], ENTITY_FAR), (&[0xE9][..], ENTITY_OUTSIDE)] {
        c.extend_from_slice(prefix);
        let end = ENTITY_TEST + c.len() as u32 + 4;
        c.extend_from_slice(&rel32(end, target));
    }
    assert!(c.len() <= ENTITY_TEST_ORIG.len());
    c.resize(ENTITY_TEST_ORIG.len(), 0x90);
    c
}

/// FUN_00a5bd10's verdict for an enabled component: [`OUTSIDE`] the loaded world (vanilla's
/// AABB test, plus the chunk test with two or more views), [`FAR`] if the squared distance
/// to the nearest view centre (vanilla: the camera centre) exceeds `distance`^2, else
/// [`KEEP`]. NaN positions are outside, a NaN distance keeps, as in vanilla.
extern "C" fn entity_verdict(entity: u32, component: u32) -> u32 {
    unsafe {
        let (x, y) = entity_pos(entity);
        let cm = chunk_map();
        let b = (cm + 0x20) as *const i32;
        let inside = x >= *b as f32 && x < *b.add(2) as f32 && y >= *b.add(1) as f32 && y < *b.add(3) as f32;
        if !inside || in_hole(cm, x, y) {
            return OUTSIDE;
        }
        let limit = *((component + 0x4c) as *const f32);
        let d2 = match views::nearest(x, y) {
            Some((_, d2)) => d2,
            None => {
                let (cx, cy) = camera_centre();
                let (dx, dy) = (cx - x, cy - y);
                dy * dy + dx * dx
            }
        };
        if d2 > limit * limit {
            FAR
        } else {
            KEEP
        }
    }
}

// --- max_count culling: FUN_00a5bbe0 ----------------------------------------------------------

/// FUN_00a5bbe0 (vtable+0x34; ESI = entity, EBX = component, EDI = &per-file entity vector
/// at 0x012094e8 + 12 * file): `call FUN_00d68150`, MSVC `std::sort` of that vector
/// (ECX = first, EDX = last; stack, caller-popped: ideal = count, camera x, camera y) by
/// ascending squared distance to the camera centre. Entries from `max_count` (+0x54) on
/// are then killed.
const CULL_SORT_CALL: u32 = 0x00a5_bc7f;
const CULL_SORT_FN: u32 = 0x00d6_8150;
/// push ebp; mov ebp,esp; sub esp,0x10; push ebx; push esi
const CULL_SORT_PROLOGUE: [u8; 8] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x10, 0x53, 0x56];
/// cdecl (first, last, ideal, x, y) thunk to FUN_00d68150.
static VANILLA_SORT: AtomicU32 = AtomicU32::new(0);

/// The per-file entity vector sort: by distance to the nearest view centre. With one view
/// that is the engine's own sort around the view centre; with more, a port of it
/// ([`msvc_sort`]) with the nearest-view key, so equal keys stay where vanilla's sort puts
/// them. Without views: vanilla (the camera centre the caller passed).
extern "C" fn cull_sort(first: u32, last: u32, ideal: i32, x: f32, y: f32) {
    unsafe {
        let n = views::count();
        if n <= 1 {
            let (x, y) = if n == 1 { views::centre(0) } else { (x, y) };
            let sort: extern "C" fn(u32, u32, i32, f32, f32) = std::mem::transmute(VANILLA_SORT.load(Ordering::Relaxed) as usize);
            sort(first, last, ideal, x, y);
            return;
        }
        if last <= first {
            return;
        }
        let v = std::slice::from_raw_parts_mut(first as *mut u32, ((last - first) / 4) as usize);
        let key = |e: u32| {
            let (x, y) = entity_pos(e);
            views::nearest(x, y).map_or(0.0, |(_, d2)| d2)
        };
        let less = |a: u32, b: u32| key(a) < key(b);
        let len = v.len();
        msvc_sort(v, 0, len, ideal, &less);
    }
}

/// MSVC's `std::sort` (`_Sort_unchecked` as compiled at FUN_00d68150: insertion sort up to
/// 32 elements, median-of-three / ninther partition, heap sort once `ideal` is spent).
pub(crate) fn msvc_sort(v: &mut [u32], mut first: usize, mut last: usize, mut ideal: i32, less: &impl Fn(u32, u32) -> bool) {
    loop {
        let count = last - first;
        if count <= 32 {
            if count >= 2 {
                insertion_sort(v, first, last, less);
            }
            return;
        }
        if ideal <= 0 {
            make_heap(v, first, last, less);
            sort_heap(v, first, last, less);
            return;
        }
        let (mid_first, mid_last) = partition(v, first, last, less);
        ideal = (ideal >> 1) + (ideal >> 2);
        if mid_first - first < last - mid_last {
            msvc_sort(v, first, mid_first, ideal, less);
            first = mid_last;
        } else {
            msvc_sort(v, mid_last, last, ideal, less);
            last = mid_first;
        }
    }
}

/// `_Insertion_sort_unchecked` (FUN_00d82f80).
fn insertion_sort(v: &mut [u32], first: usize, last: usize, less: &impl Fn(u32, u32) -> bool) {
    for next in first + 1..last {
        let val = v[next];
        if less(val, v[first]) {
            v.copy_within(first..next, first + 1);
            v[first] = val;
        } else {
            let mut hole = next;
            while hole > first + 1 && less(val, v[hole - 1]) {
                v[hole] = v[hole - 1];
                hole -= 1;
            }
            v[hole] = val;
        }
    }
}

/// `_Med3_unchecked`.
fn med3(v: &mut [u32], a: usize, m: usize, b: usize, less: &impl Fn(u32, u32) -> bool) {
    if less(v[m], v[a]) {
        v.swap(m, a);
    }
    if less(v[b], v[m]) {
        v.swap(b, m);
        if less(v[m], v[a]) {
            v.swap(m, a);
        }
    }
}

/// `_Guess_median_unchecked` (FUN_00d829a0; `last` inclusive): Tukey's ninther above 40.
fn guess_median(v: &mut [u32], first: usize, mid: usize, last: usize, less: &impl Fn(u32, u32) -> bool) {
    let count = last - first;
    if count > 40 {
        let step = (count + 1) >> 3;
        let two_step = step << 1;
        med3(v, first, first + step, first + two_step, less);
        med3(v, mid - step, mid, mid + step, less);
        med3(v, last - two_step, last - step, last, less);
        med3(v, first + step, mid, last - step, less);
    } else {
        med3(v, first, mid, last, less);
    }
}

/// `_Partition_by_median_guess_unchecked` (FUN_00d7f4e0): the range of elements equal to
/// the pivot.
fn partition(v: &mut [u32], first: usize, last: usize, less: &impl Fn(u32, u32) -> bool) -> (usize, usize) {
    let mid = first + ((last - first) >> 1);
    guess_median(v, first, mid, last - 1, less);
    let mut pfirst = mid;
    let mut plast = pfirst + 1;
    while first < pfirst && !less(v[pfirst - 1], v[pfirst]) && !less(v[pfirst], v[pfirst - 1]) {
        pfirst -= 1;
    }
    while plast < last && !less(v[plast], v[pfirst]) && !less(v[pfirst], v[plast]) {
        plast += 1;
    }
    let mut gfirst = plast;
    let mut glast = pfirst;
    loop {
        while gfirst < last {
            if less(v[pfirst], v[gfirst]) {
            } else if less(v[gfirst], v[pfirst]) {
                break;
            } else {
                if plast != gfirst {
                    v.swap(plast, gfirst);
                }
                plast += 1;
            }
            gfirst += 1;
        }
        while first < glast {
            if less(v[glast - 1], v[pfirst]) {
            } else if less(v[pfirst], v[glast - 1]) {
                break;
            } else {
                pfirst -= 1;
                if pfirst != glast - 1 {
                    v.swap(pfirst, glast - 1);
                }
            }
            glast -= 1;
        }
        if glast == first && gfirst == last {
            return (pfirst, plast);
        }
        if glast == first {
            // no room at the bottom: rotate the pivot upward
            if plast != gfirst {
                v.swap(pfirst, plast);
            }
            plast += 1;
            v.swap(pfirst, gfirst);
            pfirst += 1;
            gfirst += 1;
        } else if gfirst == last {
            // no room at the top: rotate the pivot downward
            glast -= 1;
            pfirst -= 1;
            if glast != pfirst {
                v.swap(glast, pfirst);
            }
            plast -= 1;
            v.swap(pfirst, plast);
        } else {
            glast -= 1;
            v.swap(gfirst, glast);
            gfirst += 1;
        }
    }
}

/// `_Push_heap_by_index` (FUN_00d979d0) on the heap at `base`.
fn push_heap_by_index(v: &mut [u32], base: usize, mut hole: usize, top: usize, val: u32, less: &impl Fn(u32, u32) -> bool) {
    while top < hole {
        let parent = (hole - 1) >> 1;
        if !less(v[base + parent], val) {
            break;
        }
        v[base + hole] = v[base + parent];
        hole = parent;
    }
    v[base + hole] = val;
}

/// `_Pop_heap_hole_by_index` (FUN_00d96070) on the heap at `base`.
fn pop_heap_hole_by_index(v: &mut [u32], base: usize, mut hole: usize, bottom: usize, val: u32, less: &impl Fn(u32, u32) -> bool) {
    let top = hole;
    let mut idx = 2 * hole + 2;
    while idx < bottom {
        if less(v[base + idx], v[base + idx - 1]) {
            idx -= 1;
        }
        v[base + hole] = v[base + idx];
        hole = idx;
        idx = 2 * idx + 2;
    }
    if idx == bottom {
        v[base + hole] = v[base + bottom - 1];
        hole = bottom - 1;
    }
    push_heap_by_index(v, base, hole, top, val, less);
}

/// `_Make_heap_unchecked` (FUN_00d82e30).
fn make_heap(v: &mut [u32], first: usize, last: usize, less: &impl Fn(u32, u32) -> bool) {
    let bottom = last - first;
    let mut hole = bottom >> 1;
    while hole > 0 {
        hole -= 1;
        let val = v[first + hole];
        pop_heap_hole_by_index(v, first, hole, bottom, val, less);
    }
}

/// `_Sort_heap_unchecked` (FUN_00d82f20).
fn sort_heap(v: &mut [u32], first: usize, last: usize, less: &impl Fn(u32, u32) -> bool) {
    let mut n = last - first;
    while n >= 2 {
        let val = v[first + n - 1];
        v[first + n - 1] = v[first];
        pop_heap_hole_by_index(v, first, 0, n - 1, val, less);
        n -= 1;
    }
}

// --- respawn: FUN_00a5be10, FUN_00a590f0, FUN_00a5c730 ------------------------------------------

/// FUN_00a590f0(i64 cell key, float x, float y; XMM1 = inner^2), cdecl: respawns the frozen
/// records of one cell (`push ebp; mov ebp,esp; push -1`).
const RESPAWN_CELL_FN: u32 = 0x00a5_90f0;
const RESPAWN_CELL_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
/// cdecl (key lo, key hi, x, y, inner^2) thunk to FUN_00a590f0.
static RESPAWN_CELL: AtomicU32 = AtomicU32::new(0);

/// Inside FUN_00a590f0, per record (EAX = &record.{x, y}, EDX = record offset, ECX = records
/// base, XMM2 = inner^2): 31 bytes computing XMM1 = (y - [ebp+0x14])^2 + (x - [ebp+0x10])^2,
/// the squared distance to the passed camera centre. Then `movss xmm0,[edx+ecx+0xc]`
/// ((distance - distance_border)^2); respawn iff inner^2 < d^2 < that.
const RESPAWN_DIST: u32 = 0x00a5_92f3;
const RESPAWN_DIST_ORIG: [u8; 31] = [
    0xF3, 0x0F, 0x10, 0x00, 0xF3, 0x0F, 0x10, 0x48, 0x04, 0xF3, 0x0F, 0x5C, 0x45, 0x10, 0xF3, 0x0F, 0x5C, 0x4D, 0x14, 0xF3,
    0x0F, 0x59, 0xC0, 0xF3, 0x0F, 0x59, 0xC9, 0xF3, 0x0F, 0x58, 0xC8,
];

/// FUN_00a5be10 (system update, vtable+0x28) after building its static offset vector:
/// `sub edx,edi; xor ebx,ebx; sar edx,3; test edx,edx; jz done`, then for every offset
/// FUN_00a590f0(key(camera + offset), camera x, camera y; XMM1 = inner^2). Locals:
/// [esp+0x20] camera x, [esp+0x24] camera y, [esp+0x28] inner^2 = (0.6 * max(w, h))^2.
const RESPAWN_GRID: u32 = 0x00a5_c2af;
const RESPAWN_GRID_ORIG: [u8; 15] = [0x2B, 0xD7, 0x33, 0xDB, 0xC1, 0xFA, 0x03, 0x85, 0xD2, 0x0F, 0x84, 0x81, 0x00, 0x00, 0x00];
/// The function's epilogue (SEH unlink, `pop`s, `ret`).
const RESPAWN_GRID_DONE: u32 = 0x00a5_c33f;
/// The static offset vector (begin, end; `float[2]` entries): (0,0), (±512,0), (0,±512),
/// the four ±512 diagonals, then the rest of the ±1024 5x5 grid, x-major.
const GRID_OFFSETS: u32 = 0x0122_7b04;

/// FUN_00a5c730 (no arguments, plain `ret`): after a world load (DeathMatch+0x7c, once the
/// camera's chunks are loaded) and after placing a spawned player (DeathMatch+0xbc), both in
/// DeathMatch::Update before the simulation window, it respawns the records of the 3x3
/// cells around the camera centre with inner^2 = 0 (on screen too, no AABB test).
const START_RESPAWN_FN: u32 = 0x00a5_c730;
/// push ebp; mov ebp,esp; push -1
const START_RESPAWN_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];

/// Cells per view at most (the 5x5 grid).
const GRID_CELLS: usize = 25;

/// The bucket key of the cell containing (x, y): `floor(v / 512)` through the CRT float to
/// int64 conversion (low dword; 0 for NaN / out of range), `cy * 2000 + cx` wrapping, then
/// sign-extended (`cdq`).
fn cell_key(x: f32, y: f32) -> i64 {
    fn cell(v: f32) -> i32 {
        let f = (v * (1.0 / 512.0)).floor();
        if !(-9.223_372e18..9.223_372e18).contains(&f) {
            return 0;
        }
        (f as i64) as i32
    }
    cell(y).wrapping_mul(2000).wrapping_add(cell(x)) as i64
}

unsafe fn respawn_cell(key: i64, x: f32, y: f32, inner2: f32) {
    let f: extern "C" fn(u32, u32, f32, f32, f32) = std::mem::transmute(RESPAWN_CELL.load(Ordering::Relaxed) as usize);
    f(key as u32, (key >> 32) as u32, x, y, inner2);
}

/// Respawn the records of the cells at `offsets` around every view centre: slot order, then
/// `offsets` order, each cell once. Without views: around the camera centre (x, y).
unsafe fn respawn_cells(offsets: &[[f32; 2]], x: f32, y: f32, inner2: f32) {
    let n = views::count();
    if n == 0 {
        for o in offsets {
            respawn_cell(cell_key(o[0] + x, o[1] + y), x, y, inner2);
        }
        return;
    }
    let mut done = [0i64; GRID_CELLS * views::MAX_VIEWS];
    let mut used = 0;
    for i in 0..n {
        let (vx, vy) = views::centre(i);
        for o in offsets {
            let key = cell_key(o[0] + vx, o[1] + vy);
            if done[..used].contains(&key) {
                continue;
            }
            if used < done.len() {
                done[used] = key;
                used += 1;
            }
            respawn_cell(key, vx, vy, inner2);
        }
    }
}

/// FUN_00a5be10's respawn loop: the static 5x5 grid around every view centre.
extern "C" fn respawn_grid(x: f32, y: f32, inner2: f32) {
    unsafe {
        let (begin, end) = (rd(GRID_OFFSETS), rd(GRID_OFFSETS + 4));
        if end <= begin {
            return;
        }
        let offsets = std::slice::from_raw_parts(begin as *const [f32; 2], ((end - begin) / 8) as usize);
        respawn_cells(&offsets[..offsets.len().min(GRID_CELLS)], x, y, inner2);
    }
}

/// FUN_00a590f0's squared distance from a frozen record at `pos` to the nearest view centre
/// (vanilla: to the passed camera centre (x, y)), as f32 bits. With two or more views a
/// record in an unloaded chunk gets +inf (never respawns: it would land in a hole of the
/// loaded world, outside every player's streamed area, and be frozen again).
extern "C" fn respawn_dist2(pos: *const f32, x: f32, y: f32) -> u32 {
    unsafe {
        let (px, py) = (*pos, *pos.add(1));
        let d2 = match views::nearest(px, py) {
            Some(_) if in_hole(chunk_map(), px, py) => f32::INFINITY,
            Some((_, d2)) => d2,
            None => {
                let (dx, dy) = (px - x, py - y);
                dy * dy + dx * dx
            }
        };
        d2.to_bits()
    }
}

/// FUN_00a5c730 with views: nothing (returns 1; 0 = run vanilla). It runs outside the
/// simulation window, on this client's RNG stream, at a frame chosen by this client's render
/// camera (the "camera chunks loaded" test), so respawning there would fork the clients; the
/// in-window 5x5 grid ([`respawn_grid`]) respawns the same records deterministically once
/// they are off every player's screen.
extern "C" fn start_respawn() -> u32 {
    (views::count() > 0) as u32
}

/// CameraBoundSystem per player (module docs): the per-entity freeze/kill test, the
/// max_count sort, the respawn grid and the respawn distance use every player's view.
pub(crate) unsafe fn patch_camera_bound(ex: &mut Exec) -> Result<(), PatchError> {
    expect(ENTITY_TEST, &ENTITY_TEST_ORIG)?;
    let sort_rel = rel32(CULL_SORT_CALL + 5, CULL_SORT_FN);
    expect(CULL_SORT_CALL, &[0xE8, sort_rel[0], sort_rel[1], sort_rel[2], sort_rel[3]])?;
    expect(CULL_SORT_FN, &CULL_SORT_PROLOGUE)?;
    expect(RESPAWN_CELL_FN, &RESPAWN_CELL_PROLOGUE)?;
    expect(RESPAWN_DIST, &RESPAWN_DIST_ORIG)?;
    expect(RESPAWN_GRID, &RESPAWN_GRID_ORIG)?;
    expect(START_RESPAWN_FN, &START_RESPAWN_PROLOGUE)?;

    // Thunks into the engine.
    // mov ecx,[esp+4]; mov edx,[esp+8]; push [esp+0x14] x3 (y, x, ideal); call FUN_00d68150;
    // add esp,0xc; ret
    let thunk = put_rel(
        ex,
        &[0x8B, 0x4C, 0x24, 0x04, 0x8B, 0x54, 0x24, 0x08, 0xFF, 0x74, 0x24, 0x14, 0xFF, 0x74, 0x24, 0x14, 0xFF, 0x74, 0x24, 0x14, 0xE8],
        CULL_SORT_FN,
        &[0x83, 0xC4, 0x0C, 0xC3],
    )?;
    VANILLA_SORT.store(thunk, Ordering::Relaxed);
    // movss xmm1,[esp+0x14]; jmp FUN_00a590f0 (cdecl: the extra argument is ignored)
    let thunk = put_rel(ex, &[0xF3, 0x0F, 0x10, 0x4C, 0x24, 0x14, 0xE9], RESPAWN_CELL_FN, &[])?;
    RESPAWN_CELL.store(thunk, Ordering::Relaxed);

    // Per-entity test, in place.
    write_code(ENTITY_TEST, &entity_test_code())?;

    // max_count sort: call stub; stub: push [esp+0xc] x3 (y, x, ideal); push edx; push ecx;
    // mov eax,cull_sort; call eax; add esp,0x14; ret
    let f = ((cull_sort as extern "C" fn(u32, u32, i32, f32, f32)) as u32).to_le_bytes();
    let stub = ex.put(&[
        0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x52, 0x51, 0xB8, f[0], f[1], f[2], f[3], 0xFF,
        0xD0, 0x83, 0xC4, 0x14, 0xC3,
    ]);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(CULL_SORT_CALL + 5, stub));
    write_code(CULL_SORT_CALL, &call)?;

    // Respawn distance: call stub; NOP x26. Stub (keeps EAX, ECX, EDX, XMM2; out XMM1):
    // push eax; push ecx; push edx; sub esp,4; movss [esp],xmm2; push [ebp+0x14];
    // push [ebp+0x10]; push eax; mov eax,respawn_dist2; call eax; add esp,0xc;
    // movd xmm1,eax; movss xmm2,[esp]; add esp,4; pop edx; pop ecx; pop eax; ret
    let f = ((respawn_dist2 as extern "C" fn(*const f32, f32, f32) -> u32) as u32).to_le_bytes();
    let stub = ex.put(&[
        0x50, 0x51, 0x52, 0x83, 0xEC, 0x04, 0xF3, 0x0F, 0x11, 0x14, 0x24, 0xFF, 0x75, 0x14, 0xFF, 0x75, 0x10, 0x50, 0xB8, f[0],
        f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x0C, 0x66, 0x0F, 0x6E, 0xC8, 0xF3, 0x0F, 0x10, 0x14, 0x24, 0x83, 0xC4, 0x04,
        0x5A, 0x59, 0x58, 0xC3,
    ]);
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(RESPAWN_DIST + 5, stub));
    call.resize(RESPAWN_DIST_ORIG.len(), 0x90);
    write_code(RESPAWN_DIST, &call)?;

    // Respawn grid: jmp stub; NOP x10. Stub: push [esp+0x28] x3 (inner^2, y, x);
    // mov eax,respawn_grid; call eax; add esp,0xc; jmp done
    let f = ((respawn_grid as extern "C" fn(f32, f32, f32)) as u32).to_le_bytes();
    let head = [
        0xFF, 0x74, 0x24, 0x28, 0xFF, 0x74, 0x24, 0x28, 0xFF, 0x74, 0x24, 0x28, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83,
        0xC4, 0x0C, 0xE9,
    ];
    let stub = put_rel(ex, &head, RESPAWN_GRID_DONE, &[])?;
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(RESPAWN_GRID + 5, stub));
    jmp.resize(RESPAWN_GRID_ORIG.len(), 0x90);
    write_code(RESPAWN_GRID, &jmp)?;

    // Load/spawn respawn: jmp stub. Stub: mov eax,start_respawn; call eax; test eax,eax;
    // jz +1; ret; push ebp; mov ebp,esp; push -1; jmp FUN_00a5c730+5
    let f = ((start_respawn as extern "C" fn() -> u32) as u32).to_le_bytes();
    let mut head = vec![0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x85, 0xC0, 0x74, 0x01, 0xC3];
    head.extend_from_slice(&START_RESPAWN_PROLOGUE);
    head.push(0xE9);
    let stub = put_rel(ex, &head, START_RESPAWN_FN + 5, &[])?;
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(START_RESPAWN_FN + 5, stub));
    write_code(START_RESPAWN_FN, &jmp)
}

/// Put `head` (ending in a `call`/`jmp` rel32 opcode), the rel32 to `target`, then `tail`
/// into the stub area; returns the stub's address.
unsafe fn put_rel(ex: &mut Exec, head: &[u8], target: u32, tail: &[u8]) -> Result<u32, PatchError> {
    let mut code = head.to_vec();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(tail);
    let at = ex.put(&code);
    let end = at + head.len() as u32 + 4;
    write_code(end - 4, &rel32(end, target))?;
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The port sorts like any correct sort (keys only; tie order is MSVC's).
    #[test]
    fn msvc_sort_sorts() {
        let mut seed = 0x1234_5678u32;
        for len in [0usize, 1, 2, 5, 32, 33, 41, 100, 1000] {
            let mut v: Vec<u32> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(0x343fd).wrapping_add(0x269ec3);
                    (seed >> 16) % 50
                })
                .collect();
            let n = v.len();
            msvc_sort(&mut v, 0, n, n as i32, &|a, b| a < b);
            assert!(v.windows(2).all(|w| w[0] <= w[1]), "len {len}");
            // ideal 0: heap sort
            v.reverse();
            msvc_sort(&mut v, 0, n, 0, &|a, b| a < b);
            assert!(v.windows(2).all(|w| w[0] <= w[1]), "heap len {len}");
        }
    }

    #[test]
    fn cell_keys() {
        assert_eq!(cell_key(0.0, 0.0), 0);
        assert_eq!(cell_key(511.9, 0.0), 0);
        assert_eq!(cell_key(512.0, 0.0), 1);
        assert_eq!(cell_key(-0.5, 0.0), -1);
        assert_eq!(cell_key(0.0, -1.0), -2000);
        assert_eq!(cell_key(f32::NAN, 0.0), 0);
    }
}
