//! Direct reads of Noita's (build 2025-01-25, PE timestamp 0x6794ee3c) in-memory world.
//!
//! Layout (verified at runtime): `*(0x0122374c)` = GameGlobal*; GameGlobal+0x0c = WorldData*;
//! WorldData+0x44 = GridWorld*; GridWorld+0x500 = ChunkMap; ChunkMap+0x08 = chunk table of
//! 512x512 entries (one per 512x512-pixel chunk); a non-null entry points at a struct whose
//! first dword is `Cell*[512*512]`; Cell+0x14 = CellData* (material). Material id =
//! (CellData* - CellFactory.materials_begin) / 0x290, CellFactory = *(GameGlobal+0x18),
//! materials_begin = *(CellFactory+0x18).

pub const GAME_GLOBAL_PTR: usize = 0x0122_374c;
pub const CHUNKS_PER_AXIS: usize = 512;
pub const CHUNK_CELLS: usize = 512 * 512;
const CELLDATA_SIZE: usize = 0x290;

#[derive(Clone, Copy)]
pub struct WorldPtrs {
    pub chunk_table: *const usize,
    pub materials_begin: usize,
}

/// Locate the chunk table. Returns None while the world does not exist yet.
///
/// # Safety
/// Must run inside the Noita process (the fixed address is only valid there).
pub unsafe fn world_ptrs() -> Option<WorldPtrs> {
    let gg = *(GAME_GLOBAL_PTR as *const usize);
    if gg == 0 {
        return None;
    }
    let world_data = *((gg + 0x0c) as *const usize);
    if world_data == 0 {
        return None;
    }
    let grid_world = *((world_data + 0x44) as *const usize);
    if grid_world == 0 {
        return None;
    }
    let chunk_table = *((grid_world + 0x500 + 0x08) as *const usize) as *const usize;
    let cell_factory = *((gg + 0x18) as *const usize);
    if chunk_table.is_null() || cell_factory == 0 {
        return None;
    }
    let materials_begin = *((cell_factory + 0x18) as *const usize);
    Some(WorldPtrs { chunk_table, materials_begin })
}

/// 64-bit mixing hash step (splitmix-style finalizer over an accumulator).
#[inline(always)]
pub fn mix(h: u64, v: u64) -> u64 {
    let mut x = h ^ v.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^ (x >> 29)
}

/// Hash one chunk's cells: every non-empty cell contributes (index, material id).
///
/// # Safety
/// `cells` must point at a live `Cell*[CHUNK_CELLS]` array of the running game.
pub unsafe fn hash_cells(cells: *const usize, materials_begin: usize) -> u64 {
    let mut h: u64 = 0x243f_6a88_85a3_08d3;
    for i in 0..CHUNK_CELLS {
        let c = *cells.add(i);
        if c != 0 {
            let mat = *((c + 0x14) as *const usize);
            let id = mat.wrapping_sub(materials_begin) / CELLDATA_SIZE;
            h = mix(h, ((i as u64) << 32) | id as u64);
        }
    }
    h
}

/// Visit every loaded chunk: (chunk table index, Cell* array).
///
/// # Safety
/// See [`world_ptrs`].
pub unsafe fn for_each_chunk(w: &WorldPtrs, mut f: impl FnMut(u32, *const usize)) {
    for idx in 0..CHUNKS_PER_AXIS * CHUNKS_PER_AXIS {
        let chunk = *w.chunk_table.add(idx);
        if chunk != 0 {
            let cells = *(chunk as *const usize) as *const usize;
            if !cells.is_null() {
                f(idx as u32, cells);
            }
        }
    }
}

/// Hash all loaded chunks.
///
/// # Safety
/// See [`world_ptrs`]. Must be called on the game thread between simulation steps.
pub unsafe fn hash_loaded_chunks() -> Vec<protocol::ChunkHash> {
    let Some(w) = world_ptrs() else { return Vec::new() };
    let mut out = Vec::with_capacity(32);
    for_each_chunk(&w, |index, cells| {
        out.push(protocol::ChunkHash { index, hash: hash_cells(cells, w.materials_begin) });
    });
    out
}
