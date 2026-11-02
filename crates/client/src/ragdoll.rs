//! Ragdoll stains: vanilla's blurred overlay, computed from simulation state.
//!
//! When a creature dies, the ragdoll loader `FUN_00be8240` blends an overlay image into the
//! ragdoll images (the blend becomes the ragdoll bodies' pixels, so it shapes their fixtures
//! and mass). Vanilla's overlay is SpriteStains+0x20: a horizontally blurred copy of the stain
//! colours at SpriteStains+0x00, written only by the stains texture upload `FUN_00c21020`.
//! That upload runs from the render-side queue and only for sprites drawn last frame, so the
//! overlay held each client's last *drawn* stains. [`patch_ragdoll_stains_blur`] passes the
//! same blur of the *current* stain colours instead, computed here exactly as the upload
//! computes it: a pure function of +0x00 (simulation state), identical on every client.
//!
//! Vanilla nuance kept out: +0x20 can lag the stains by the upload queue (at most 3 uploads
//! per frame, only while on screen), and is all zero for a creature never drawn. The
//! deterministic version is always the blur of the stains at the moment of death.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::patches::{expect, rel32, write_code, Exec, PatchError};

/// Ragdoll creation on death (FUN_00abb7d0): `mov eax,[eax+0x5c]; test eax,eax; jz +7;
/// add eax,0x20; mov [esp+0x34],eax` (eax = SpriteStainsComponent, +0x5c = SpriteStains)
/// picks the overlay that both ragdoll loads (`call FUN_00be8240` at 0x00abc27e and
/// 0x00abd376, each `push dword [esp+0x34]` as its last stack argument) read.
const SITE: u32 = 0x00ab_c082;
const SITE_ORIG: [u8; 14] = [0x8B, 0x40, 0x5C, 0x85, 0xC0, 0x74, 0x07, 0x83, 0xC0, 0x20, 0x89, 0x44, 0x24, 0x34];
/// Where the site continues (`mov ecx,[esp+0x14]`), with or without an overlay.
const SITE_END: u32 = SITE + 14;

/// The engine's u32 image (FUN_0089a750 sizes it): w, h, pixel count, ..., pixels (row-major,
/// stride w, 0xAABBGGRR), allocated pixel count. The loader reads only w, h and the pixels.
#[repr(C)]
struct Image {
    w: i32,
    h: i32,
    n: i32,
    _unused: [u32; 3],
    data: *mut u32,
    cap: i32,
}

/// The overlay handed to the loader. Its pixels live until the next death on this thread;
/// the loader keeps no pointer to the overlay past its call, and nothing between the site
/// and either loader call can kill an entity (only component lookups and the GameGlobal
/// getter), so the overlay is consumed before the site runs again.
struct Scratch {
    image: Image,
    pixels: Vec<u32>,
}

thread_local! {
    static SCRATCH: UnsafeCell<Scratch> = const {
        UnsafeCell::new(Scratch {
            image: Image { w: 0, h: 0, n: 0, _unused: [0; 3], data: std::ptr::null_mut(), cap: 0 },
            pixels: Vec::new(),
        })
    };
}

/// SpriteStains of the latest overlay (for the PTRACE a8 diagnostics).
static LAST_STAINS: AtomicU32 = AtomicU32::new(0);

/// The SpriteStains whose blur `overlay` is, if `overlay` is this thread's overlay image.
pub(crate) fn stains_of(overlay: u32) -> Option<u32> {
    let ours = SCRATCH.with(|s| unsafe { std::ptr::addr_of!((*s.get()).image) as u32 });
    (overlay != 0 && overlay == ours).then(|| LAST_STAINS.load(Ordering::Relaxed))
}

/// The site jumps to a stub that, for an entity with SpriteStains, stores the blur of its
/// stain colours (`ragdoll_overlay`) as the overlay instead of SpriteStains+0x20. The
/// pixels are a scratch image of the same size and layout, so the render-side +0x20 (the
/// texture source) is left to vanilla.
pub(crate) unsafe fn patch_ragdoll_stains_blur(ex: &mut Exec) -> Result<(), PatchError> {
    expect(SITE, &SITE_ORIG)?;
    let f = ((ragdoll_overlay as extern "C" fn(u32) -> u32) as u32).to_le_bytes();
    let mut stub: Vec<u8> = vec![
        0x8B, 0x40, 0x5C, // mov eax,[eax+0x5c]
        0x85, 0xC0, // test eax,eax
        0x74, 0x00, // jz done (patched below)
        0x51, // push ecx
        0x52, // push edx
        0x81, 0xEC, 0x80, 0x00, 0x00, 0x00, // sub esp,0x80
    ];
    // movups [esp+16*i],xmmi (xmm0-7: all volatile for the callee, kept for the site)
    for i in 0..8u8 {
        stub.extend_from_slice(&[0x0F, 0x11, 0x44 | (i << 3), 0x24, i * 0x10]);
    }
    stub.extend_from_slice(&[
        0x50, // push eax (SpriteStains)
        0xB8, f[0], f[1], f[2], f[3], // mov eax,ragdoll_overlay
        0xFF, 0xD0, // call eax
        0x83, 0xC4, 0x04, // add esp,4
    ]);
    for i in 0..8u8 {
        stub.extend_from_slice(&[0x0F, 0x10, 0x44 | (i << 3), 0x24, i * 0x10]);
    }
    stub.extend_from_slice(&[
        0x81, 0xC4, 0x80, 0x00, 0x00, 0x00, // add esp,0x80
        0x5A, // pop edx
        0x59, // pop ecx
        0x89, 0x44, 0x24, 0x34, // mov [esp+0x34],eax
    ]);
    let done = stub.len();
    stub[6] = (done - 7) as u8;
    stub.extend_from_slice(&[0xE9, 0, 0, 0, 0]); // done: jmp SITE_END
    let at = ex.put(&stub);
    let end = at + stub.len() as u32;
    write_code(end - 4, &rel32(end, SITE_END))?;
    // mov eax,[eax+0x5c] ... mov [esp+0x34],eax -> jmp stub; nop x9
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(SITE + 5, at));
    jmp.extend_from_slice(&[0x90; 9]);
    write_code(SITE, &jmp)
}

/// The overlay for a dying entity's ragdoll: the blur of SpriteStains+0x00 in this
/// thread's scratch image. Grows the scratch only when a larger stain image appears.
extern "C" fn ragdoll_overlay(stains: u32) -> u32 {
    LAST_STAINS.store(stains, Ordering::Relaxed);
    SCRATCH.with(|s| unsafe {
        let s = &mut *s.get();
        let src = &*(stains as *const Image);
        let (w, h) = (src.w.max(0), src.h.max(0));
        let n = w as usize * h as usize;
        if s.pixels.len() < n {
            s.pixels.resize(n, 0);
        }
        if n != 0 {
            let colours = std::slice::from_raw_parts(src.data, n);
            blur(colours, w as usize, &mut s.pixels[..n]);
        }
        s.image = Image { w: src.w, h: src.h, n: n as i32, _unused: [0; 3], data: s.pixels.as_mut_ptr(), cap: s.pixels.len() as i32 };
        std::ptr::addr_of!(s.image) as u32
    })
}

/// One colour channel as the upload reads it: `cvtdq2pd; addsd [0x01054630 + 8*(v>>31)]`
/// (0.0 for v <= 255); `cvtpd2ps; divss 255.0` — all exact up to the f32 division.
#[inline(always)]
fn channel(v: u32) -> f32 {
    v as f32 / 255.0
}

/// FUN_00c21020's blur, pixel for pixel (`src`/`dst` row-major, stride `w`):
///
/// ```text
/// c = src[y][x] / 255 (per channel r,g,b,a), weight = 1.0
/// if x > 0: l = src[y][x-1] / 255; c += l (per channel); weight = l.a + 1.0
/// if weight - 1.0 > (float)x: (right neighbour, never taken, see below)
/// dst[y][x] = pack(c / weight), pack (FUN_0044abc0) = per channel cvttss2si(v * 255.0)
///             clamped to [0, 255], 0xAABBGGRR
/// ```
///
/// The right-neighbour test compares the *weight* with the column: weight - 1.0 is 0.0 at
/// x = 0 and l.a ∈ [0, 1] (exactly 1.0 at most: 1.0 + 1.0 - 1.0) for x >= 1, never above x.
/// So vanilla only ever blends the left neighbour, and so does this.
fn blur(src: &[u32], w: usize, dst: &mut [u32]) {
    for (row_src, row_dst) in src.chunks_exact(w).zip(dst.chunks_exact_mut(w)) {
        for x in 0..w {
            let p = row_src[x];
            let mut c = [channel(p & 0xff), channel(p >> 8 & 0xff), channel(p >> 16 & 0xff), channel(p >> 24)];
            let mut weight = 1.0f32;
            if x > 0 {
                let l = row_src[x - 1];
                let la = channel(l >> 24);
                c[0] = channel(l & 0xff) + c[0];
                c[1] = channel(l >> 8 & 0xff) + c[1];
                c[2] = channel(l >> 16 & 0xff) + c[2];
                c[3] = c[3] + la;
                weight = la + 1.0;
            }
            let mut out = 0u32;
            for (i, v) in c.into_iter().enumerate() {
                // cvttss2si; in range: v / weight <= 2.0
                let q = ((v / weight) * 255.0) as i32;
                out |= (q.clamp(0, 255) as u32) << (8 * i);
            }
            row_dst[x] = out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::blur;

    #[test]
    fn blends_left_neighbour_weighted_by_its_alpha() {
        // Rows of 2: [opaque red, opaque green], [transparent black, stain], [stain, transparent black].
        let src = [0xff00_00ff, 0xff00_ff00, 0x0000_0000, 0x8040_2010, 0x8040_2010, 0x0000_0000];
        let mut dst = [0u32; 6];
        blur(&src, 2, &mut dst);
        // Green + red: r,g = (1 + 0) / 2 -> 127, a = (1 + 1) / 2 -> 255. A transparent black
        // left neighbour keeps the pixel. A row's first pixel has no left neighbour (row 3
        // starts after a stain). A stain to the left: 16/255 / (1 + 128/255) * 255 = 10.6 -> 10.
        assert_eq!(dst, [0xff00_00ff, 0xff00_7f7f, 0x0000_0000, 0x8040_2010, 0x8040_2010, 0x552a_150a]);
    }
}
