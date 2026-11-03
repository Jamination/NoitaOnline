//! The entity part of the state report, read natively: what `State.collect` (files/state.lua)
//! used to gather through the Lua API (`EntityGetInRadius(0, 0, 1e9)`, `EntityGetTransform`,
//! `EntityHasTag`), with the engine's own arithmetic, so the digest bytes are unchanged.

use crate::patches::rd;

/// EntityManager*: +0x14/+0x18 the entity pointer vector `EntityGetInRadius` walks.
const ENTITY_MANAGER: u32 = 0x0120_4b98;
/// Entity: +0 id, +0xc > 0 once killed, +0x30 tag bitset (512 bits), +0x70/+0x74 position,
/// +0x78/+0x7c rotation (sin, cos).
const ENTITY_DEAD: u32 = 0xc;
const ENTITY_TAGS: u32 = 0x30;
const ENTITY_X: u32 = 0x70;
/// `EntityGetTransform`'s rotation: FUN_0044a330 (XMM0 = +0x7c, XMM1 = +0x78; x87 `atan2`,
/// float result in XMM0).
const ROTATION_FN: u32 = 0x0044_a330;
/// Tag registry (`[0x01206fac]`) and its name -> bitset lookup FUN_008a0df0 (thiscall
/// registry, out bitset, `const std::string&`), as `EntityHasTag` uses them (FUN_0056d190 ->
/// FUN_008a0cd0). An unknown tag gives the empty bitset.
const TAG_REGISTRY: u32 = 0x0120_6fac;
const TAG_BITS_FN: u32 = 0x008a_0df0;

/// One entity, as `State.collect` hashes it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EntityRec {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub rot: f32,
    /// 0 `noita_online_player`, 1 `enemy`, 2 other.
    pub kind: u32,
}

/// MSVC 2013 `std::string` (x86): 16-byte buffer or heap pointer, size, capacity.
#[repr(C)]
struct MsvcString {
    buf: [u32; 4],
    size: u32,
    capacity: u32,
}

unsafe fn tag_bits(name: &'static [u8]) -> [u32; 16] {
    let mut s = MsvcString { buf: [0; 4], size: name.len() as u32 - 1, capacity: 0 };
    if name.len() <= 16 {
        std::ptr::copy_nonoverlapping(name.as_ptr(), s.buf.as_mut_ptr() as *mut u8, name.len());
        s.capacity = 15;
    } else {
        s.buf[0] = name.as_ptr() as u32;
        s.capacity = s.size;
    }
    let mut out = [0u32; 16];
    let registry = rd(TAG_REGISTRY);
    if registry != 0 {
        let f: extern "thiscall" fn(u32, *mut [u32; 16], *const MsvcString) -> u32 = std::mem::transmute(TAG_BITS_FN as usize);
        f(registry, &mut out, &s);
    }
    out
}

/// `EntityHasTag` (FUN_008a0d60): every bit of the tag's set is set, and the set is not empty.
unsafe fn has_tag(entity: u32, tag: &[u32; 16]) -> bool {
    let bits = (entity + ENTITY_TAGS) as *const u32;
    tag.iter().any(|&t| t != 0) && tag.iter().enumerate().all(|(k, &t)| *bits.add(k) & t == t)
}

unsafe fn rotation(entity: u32) -> f32 {
    let (sin, cos) = (*((entity + 0x78) as *const f32), *((entity + 0x7c) as *const f32));
    let r: f32;
    std::arch::asm!(
        "call {f}",
        f = in(reg) ROTATION_FN,
        inout("xmm0") cos => r,
        in("xmm1") sin,
        clobber_abi("C"),
    );
    r
}

/// Every live entity `EntityGetInRadius(0, 0, 1e9)` returns except `proxy`, sorted by id, into
/// `out`; returns their count (entries beyond `out.len()` are counted, not written).
///
/// # Safety
/// Game thread, world initialised.
pub unsafe fn snapshot(proxy: u32, out: &mut [EntityRec]) -> usize {
    let mgr = rd(ENTITY_MANAGER);
    if mgr == 0 {
        return 0;
    }
    let (player, enemy) = (tag_bits(b"noita_online_player\0"), tag_bits(b"enemy\0"));
    let (begin, end) = (rd(mgr + 0x14), rd(mgr + 0x18));
    let n = if begin > end { 0 } else { (end - begin + 3) >> 2 };
    // FUN_00795ab0: dx*dx + dy*dy against radius^2 in single precision, NaN excluded.
    let r2 = 1e9f32 * 1e9f32;
    let mut count = 0;
    for i in 0..n {
        let e = rd(begin + 4 * i);
        if e == 0 || (rd(e + ENTITY_DEAD) as i32) > 0 {
            continue;
        }
        let x = *((e + ENTITY_X) as *const f32);
        let y = *((e + ENTITY_X + 4) as *const f32);
        let (dx, dy) = (x - 0.0, y - 0.0);
        if !(r2 >= dy * dy + dx * dx) {
            continue;
        }
        let id = rd(e);
        if id == proxy {
            continue;
        }
        if let Some(slot) = out.get_mut(count) {
            let kind = if has_tag(e, &player) {
                0
            } else if has_tag(e, &enemy) {
                1
            } else {
                2
            };
            *slot = EntityRec { id, x, y, rot: rotation(e), kind };
        }
        count += 1;
    }
    if count <= out.len() {
        // `table.sort` of the ids (Lua integers).
        out[..count].sort_unstable_by_key(|r| r.id as i32);
    }
    count
}

/// Diagnostics (`NOITA_ONLINE_PROF` counters): live entities, those tagged `enemy`,
/// `projectile`, `item_pickup`, and the position of the first `player_unit` (0, 0 if none).
///
/// # Safety
/// Game thread, world initialised.
pub unsafe fn census() -> ([u64; 4], (f32, f32)) {
    let mgr = rd(ENTITY_MANAGER);
    if mgr == 0 {
        return ([0; 4], (0.0, 0.0));
    }
    let tags = [tag_bits(b"enemy\0"), tag_bits(b"projectile\0"), tag_bits(b"item_pickup\0")];
    let player = tag_bits(b"player_unit\0");
    let (begin, end) = (rd(mgr + 0x14), rd(mgr + 0x18));
    let n = if begin > end { 0 } else { (end - begin + 3) >> 2 };
    let mut counts = [0u64; 4];
    let mut pos = None;
    for i in 0..n {
        let e = rd(begin + 4 * i);
        if e == 0 || (rd(e + ENTITY_DEAD) as i32) > 0 {
            continue;
        }
        counts[0] += 1;
        for (c, t) in counts[1..].iter_mut().zip(&tags) {
            *c += has_tag(e, t) as u64;
        }
        if pos.is_none() && has_tag(e, &player) {
            pos = Some((*((e + ENTITY_X) as *const f32), *((e + ENTITY_X + 4) as *const f32)));
        }
    }
    (counts, pos.unwrap_or((0.0, 0.0)))
}
