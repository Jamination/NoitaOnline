//! Direct reads of Noita's (build 2025-01-25, PE timestamp 0x6794ee3c) in-memory world.
//!
//! Layout (verified at runtime): `*(0x0122374c)` = GameGlobal*; GameGlobal+0x0c = WorldData*;
//! WorldData+0x44 = GridWorld*; GridWorld+0x500 = ChunkMap; ChunkMap+0x08 = chunk table of
//! 512x512 entries (one per 512x512-pixel chunk); a non-null entry points at a struct whose
//! first dword is `Cell*[512*512]`; Cell+0x14 = CellData* (material). Materials are a
//! contiguous table of 0x290-byte CellData starting at materials_begin =
//! *(CellFactory+0x18), CellFactory = *(GameGlobal+0x18).

pub const GAME_GLOBAL_PTR: usize = 0x0122_374c;
pub const CHUNKS_PER_AXIS: usize = 512;
pub const CHUNK_CELLS: usize = 512 * 512;

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

/// Hash one chunk's cells: every non-empty cell contributes (index, material). The
/// material is identified by its byte offset into the material table (identical in every
/// process; avoids a division per cell).
///
/// # Safety
/// `cells` must point at a live `Cell*[CHUNK_CELLS]` array of the running game.
pub unsafe fn hash_cells(cells: *const usize, materials_begin: usize) -> u64 {
    const AHEAD: usize = 24;
    let mut h: u64 = 0x243f_6a88_85a3_08d3;
    for i in 0..CHUNK_CELLS {
        if i + AHEAD < CHUNK_CELLS {
            let next = *cells.add(i + AHEAD);
            if next != 0 {
                prefetch((next + 0x14) as *const u8);
            }
        }
        let c = *cells.add(i);
        if c != 0 {
            let mat = *((c + 0x14) as *const usize);
            let off = mat.wrapping_sub(materials_begin);
            h = (h ^ (((i as u64) << 32) | off as u64)).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
        }
    }
    mix(h, 0)
}

#[inline(always)]
pub(crate) unsafe fn prefetch(p: *const u8) {
    #[cfg(target_arch = "x86")]
    core::arch::x86::_mm_prefetch(p as *const i8, core::arch::x86::_MM_HINT_T0);
    #[cfg(target_arch = "x86_64")]
    core::arch::x86_64::_mm_prefetch(p as *const i8, core::arch::x86_64::_MM_HINT_T0);
}

/// Visit every loaded chunk: (chunk table index, Cell* array).
///
/// # Safety
/// See [`world_ptrs`].
pub unsafe fn for_each_chunk(w: &WorldPtrs, f: impl FnMut(u32, *const usize)) {
    for_each_chunk_strided(w, 0, 1, f)
}

/// Visit the loaded chunks with table index `first`, `first + step`, ... (the staggered
/// hash visits 1/60 of the 1 MiB table per frame instead of all of it).
///
/// # Safety
/// See [`world_ptrs`].
pub unsafe fn for_each_chunk_strided(w: &WorldPtrs, first: usize, step: usize, mut f: impl FnMut(u32, *const usize)) {
    for idx in (first..CHUNKS_PER_AXIS * CHUNKS_PER_AXIS).step_by(step.max(1)) {
        let chunk = *w.chunk_table.add(idx);
        if chunk != 0 {
            let cells = *(chunk as *const usize) as *const usize;
            if !cells.is_null() {
                f(idx as u32, cells);
            }
        }
    }
}

/// The loaded chunks with table index `first`, `first + step`, ... in ascending order, as
/// (chunk table index, `Cell*` array address), and the material table base for
/// [`hash_chunks`]. Empty while the world does not exist.
///
/// # Safety
/// See [`world_ptrs`].
pub unsafe fn loaded_chunks(first: usize, step: usize) -> (Vec<(u32, usize)>, usize) {
    let Some(w) = world_ptrs() else { return (Vec::new(), 0) };
    let mut chunks: Vec<(u32, usize)> = Vec::with_capacity(32);
    for_each_chunk_strided(&w, first, step, |index, cells| chunks.push((index, cells as usize)));
    (chunks, w.materials_begin)
}

/// Hash chunks listed by [`loaded_chunks`]. A few chunks (the staggered report's 0-2 per
/// frame) are hashed on the calling thread, the report worker; only big batches (every
/// loaded chunk at once, `NOITA_ONLINE_HASH=full`) are spread over a few threads, since
/// spawning threads costs more than a chunk (under Wine every new thread is a round of
/// wineserver calls and DLL thread notifications).
///
/// # Safety
/// The chunks must still be loaded and their cells unchanged since they were listed, i.e.
/// no simulation step may run until this returns.
pub unsafe fn hash_chunks(chunks: &[(u32, usize)], materials_begin: usize) -> Vec<protocol::ChunkHash> {
    const SPAWN_FROM: usize = 8;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4);
    let mut out: Vec<protocol::ChunkHash> = Vec::with_capacity(chunks.len());
    if threads == 1 || chunks.len() < SPAWN_FROM {
        for &(index, cells) in chunks {
            out.push(protocol::ChunkHash { index, hash: hash_cells(cells as *const usize, materials_begin) });
        }
        return out;
    }
    let per = chunks.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .chunks(per)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|&(index, cells)| protocol::ChunkHash { index, hash: hash_cells(cells as *const usize, materials_begin) })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().unwrap_or_default());
        }
    });
    out
}

/// Diagnostics: write every loaded chunk as `[index u32][material id u16 × CHUNK_CELLS]`
/// (0xffff = empty) to `path`, for cell-level comparison of two clients' worlds.
///
/// # Safety
/// See [`world_ptrs`]. Game thread, between simulation steps.
pub unsafe fn dump_chunks(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let Some(w) = world_ptrs() else { return Ok(()) };
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut err = Ok(());
    for_each_chunk(&w, |index, cells| {
        let mut buf = Vec::with_capacity(4 + 2 * CHUNK_CELLS);
        buf.extend_from_slice(&index.to_le_bytes());
        for i in 0..CHUNK_CELLS {
            let c = *cells.add(i);
            let id = if c == 0 { 0xffff } else { ((*((c + 0x14) as *const usize)).wrapping_sub(w.materials_begin) / 0x290) as u16 };
            buf.extend_from_slice(&id.to_le_bytes());
        }
        if err.is_ok() {
            err = out.write_all(&buf);
        }
    });
    err
}

/// Diagnostics: raw bytes (0x40) of every non-empty cell in the world rect [x0,x1)×[y0,y1),
/// one line per cell: `x y +00 +04 ...` (dwords, hex).
///
/// # Safety
/// See [`world_ptrs`]. Game thread, between simulation steps.
pub unsafe fn dump_cells(path: &std::path::Path, x0: i32, y0: i32, x1: i32, y1: i32) -> std::io::Result<()> {
    use std::io::Write;
    let Some(w) = world_ptrs() else { return Ok(()) };
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for y in y0..y1 {
        for x in x0..x1 {
            let idx = (((y >> 9) + 256) & 511) as usize * 512 + (((x >> 9) + 256) & 511) as usize;
            let chunk = *w.chunk_table.add(idx);
            if chunk == 0 {
                continue;
            }
            let cells = *(chunk as *const usize) as *const usize;
            let c = *cells.add((((y & 511) as usize) << 9) | (x & 511) as usize);
            if c == 0 {
                continue;
            }
            write!(out, "{x} {y} mat={}", (*((c + 0x14) as *const usize)).wrapping_sub(w.materials_begin) / 0x290)?;
            for k in 0..16 {
                write!(out, " {:08x}", *((c + 4 * k) as *const u32))?;
            }
            writeln!(out)?;
        }
    }
    Ok(())
}

/// Diagnostics: every GridParticles particle (lists +0x34/+0x40/+0x4c, 0x70 bytes each), one
/// line per particle: `list index x y` then dwords 0..27 (hex; dword 20 is the cell pointer,
/// replaced by the cell's material id). Then the side lists: `9c` (0x18-byte snapshot-colliding
/// particles, +0x9c), `c8` (0x14-byte radial forces, +0xc8), and `hdr` (GridParticles dwords
/// 0..0x40) and `snap` (GridWorld+0x474 material snapshot: width, height, FNV hash).
///
/// # Safety
/// See [`world_ptrs`]. Game thread, between simulation steps.
pub unsafe fn dump_particles(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let Some(w) = world_ptrs() else { return Ok(()) };
    let gg = *(GAME_GLOBAL_PTR as *const usize);
    let grid_world = *((*((gg + 0x0c) as *const usize) + 0x44) as *const usize);
    let gp = *((grid_world + 0x540) as *const usize);
    if gp == 0 {
        return Ok(());
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for list in [0x34usize, 0x40, 0x4c] {
        let b = *((gp + list) as *const usize);
        let e = *((gp + list + 4) as *const usize);
        for (i, p) in (b..e).step_by(0x70).enumerate() {
            let d = |k: usize| *((p + 4 * k) as *const u32);
            write!(out, "{list:x} {i} {} {}", f32::from_bits(d(7)), f32::from_bits(d(8)))?;
            for k in 0..28 {
                let v = if k == 20 && d(20) != 0 {
                    ((*((d(20) as usize + 0x14) as *const usize)).wrapping_sub(w.materials_begin) / 0x290) as u32
                } else {
                    d(k)
                };
                write!(out, " {v:08x}")?;
            }
            writeln!(out)?;
        }
    }
    for (list, stride) in [(0x9cusize, 0x18usize), (0xc8, 0x14)] {
        let b = *((gp + list) as *const usize);
        let e = *((gp + list + 4) as *const usize);
        for (i, p) in (b..e).step_by(stride).enumerate() {
            write!(out, "{list:x} {i}")?;
            for k in 0..stride / 4 {
                write!(out, " {:08x}", *((p + 4 * k) as *const u32))?;
            }
            writeln!(out)?;
        }
    }
    write!(out, "hdr")?;
    for k in 0..0x40 {
        write!(out, " {:08x}", *((gp + 4 * k) as *const u32))?;
    }
    writeln!(out)?;
    let snap = grid_world + 0x474;
    let (sw, sh, data) = (*(snap as *const u32), *((snap + 4) as *const u32), *((snap + 0x18) as *const usize));
    let mut fnv = 0x811c_9dc5u32;
    if data != 0 && sw < 4096 && sh < 4096 {
        for k in 0..(sw * sh) as usize {
            fnv = (fnv ^ *((data + 4 * k) as *const u32)).wrapping_mul(0x0100_0193);
        }
    }
    writeln!(out, "snap {sw} {sh} {fnv:08x} view {} {}", *((grid_world + 0x498) as *const i32), *((grid_world + 0x49c) as *const i32))?;
    // Physics: the b2World's body list (GridWorld+0x530 -> +0x1922c, next at body+0xb8), 0x100
    // bytes per body; then the grid-body wrappers (GridWorld+0x560 vector -> +0x11c).
    let b2 = *((grid_world + 0x530) as *const usize);
    if b2 != 0 {
        let mut body = *((b2 + 0x1922c) as *const usize);
        let mut i = 0;
        while body != 0 && i < 100_000 {
            write!(out, "body {i}")?;
            for k in 0..0x40 {
                write!(out, " {:08x}", *((body + 4 * k) as *const u32))?;
            }
            writeln!(out)?;
            body = *((body + 0xb8) as *const usize);
            i += 1;
        }
    }
    let (gb, ge) = (*((grid_world + 0x560) as *const usize), *((grid_world + 0x564) as *const usize));
    for (i, p) in (gb..ge).step_by(4).enumerate() {
        let elem = *(p as *const usize);
        let wrap = if elem != 0 && (*((elem + 0xc) as *const u8) >> 5) & 1 != 0 { *((elem + 0x11c) as *const usize) } else { 0 };
        write!(out, "gbody {i}")?;
        if wrap != 0 {
            for k in 0..0x20 {
                write!(out, " {:08x}", *((wrap + 4 * k) as *const u32))?;
            }
        }
        writeln!(out)?;
    }
    Ok(())
}
