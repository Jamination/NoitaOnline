//! Every player's view, for the engine systems that vanilla ties to its one camera.
//!
//! Vanilla Noita has one camera, centred on the local player. Many simulation systems test
//! against it: particle effects are only created on screen, CameraBound entities far from it
//! are frozen and killed, animals only start hunting near it, rain falls around it. Inside
//! the lockstep window the engine camera is the shared simulation camera (the first live
//! player's view), so those systems would run around player 1 only. Their patches test
//! against every player's view instead ("as if each player's own camera were the camera").
//!
//! A player's view is the camera that player's own client rendered: each client sends the
//! position of its render camera (the camera it drew the frame its input was captured in)
//! with every input ([`CameraEncoder`], a trailer of the input bytes), and every client
//! decodes it from the ticks in frame order ([`decode_trailer`], [`camera_of`]), so the views
//! are identical everywhere. They lag the render by the input delay plus one frame (the
//! input captured in frame f, with the camera drawn in frame f − 1, is applied in the tick
//! for f + delay): 2 frames on a LAN, a few pixels of camera motion. A player whose client has
//! sent no camera yet (only the frames before its first input) is viewed from its position.
//! A player who left or died keeps the last camera it sent.
//!
//! A view's rect is vanilla's: the simulation camera's int view rect (GridWorld+0x498 x, y,
//! w, h, the rect the render snapshots and the view-bound particle tests use), which is view
//! 0's own because the simulation camera is view 0's camera, moved by the view centre's
//! offset from view 0's (as `rain_views` and the camera loads move it).
//!
//! The views are written by the game thread inside the window before the world update and
//! read during it, also by pool workers (grid tiles); the pool's job hand-off orders them.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;

pub const MAX_VIEWS: usize = 16;
/// Vanilla's int view rect size (the camera rect's 431×242, which the render snapshots), for
/// views published before the camera rect exists.
const FALLBACK_W: i32 = 431;
const FALLBACK_H: i32 = 242;

static COUNT: AtomicU32 = AtomicU32::new(0);
static CENTRES: [[AtomicU32; 2]; MAX_VIEWS] = [const { [AtomicU32::new(0), AtomicU32::new(0)] }; MAX_VIEWS];
static RECTS: [[AtomicI32; 4]; MAX_VIEWS] = [const { [const { AtomicI32::new(0) }; 4] }; MAX_VIEWS];

/// Publish this window's views (slot order): each view's camera centre, and `base`, the
/// simulation camera's int view rect {x, y, w, h} (GridWorld+0x498), already set to view 0's
/// camera. Game thread, inside the window.
pub fn set(centres: &[(f32, f32)], base: Option<[i32; 4]>) {
    let centres = &centres[..centres.len().min(MAX_VIEWS)];
    let origin = centres.first().copied().unwrap_or((0.0, 0.0));
    for (i, &(x, y)) in centres.iter().enumerate() {
        CENTRES[i][0].store(x.to_bits(), Ordering::Relaxed);
        CENTRES[i][1].store(y.to_bits(), Ordering::Relaxed);
        let r = match base {
            Some([bx, by, w, h]) => {
                let (dx, dy) = (round_px(x - origin.0), round_px(y - origin.1));
                let (x0, y0) = (bx.wrapping_add(dx), by.wrapping_add(dy));
                [x0, y0, x0.wrapping_add(w), y0.wrapping_add(h)]
            }
            None => {
                let (x0, y0) = (round_px(x) - FALLBACK_W / 2, round_px(y) - FALLBACK_H / 2);
                [x0, y0, x0 + FALLBACK_W, y0 + FALLBACK_H]
            }
        };
        for (slot, v) in RECTS[i].iter().zip(r) {
            slot.store(v, Ordering::Relaxed);
        }
    }
    COUNT.store(centres.len() as u32, Ordering::Release);
}

/// `floor(v + 0.5)` as an int (0 for NaN, saturating).
fn round_px(v: f32) -> i32 {
    (v + 0.5).floor() as i32
}

/// Number of views (0 before the first window published the players, or when no player
/// is alive).
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire) as usize
}

/// Centre of view `i` (world px).
pub fn centre(i: usize) -> (f32, f32) {
    (f32::from_bits(CENTRES[i][0].load(Ordering::Relaxed)), f32::from_bits(CENTRES[i][1].load(Ordering::Relaxed)))
}

/// Rect of view `i`: `[x0, y0, x1, y1)` in world px.
pub fn rect(i: usize) -> [i32; 4] {
    let r = &RECTS[i];
    [r[0].load(Ordering::Relaxed), r[1].load(Ordering::Relaxed), r[2].load(Ordering::Relaxed), r[3].load(Ordering::Relaxed)]
}

/// Every view rect, slot order.
pub fn rects() -> Vec<[i32; 4]> {
    (0..count()).map(rect).collect()
}

/// Whether the world pixel (x, y) lies in some player's view.
pub fn in_any(x: i32, y: i32) -> bool {
    (0..count()).any(|i| {
        let r = rect(i);
        x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
    })
}

/// The view whose centre is nearest to (x, y), as (index, squared distance); the lowest
/// slot wins ties. None without views.
pub fn nearest(x: f32, y: f32) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for i in 0..count() {
        let (cx, cy) = centre(i);
        let d = (x - cx) * (x - cx) + (y - cy) * (y - cy);
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((i, d));
        }
    }
    best
}

/// For each row `y` of the union of all view rects (top to bottom), the row's disjoint
/// x-intervals `[x0, x1)` in ascending order: every pixel of the union exactly once.
pub fn union_rows(rects: &[[i32; 4]], mut f: impl FnMut(i32, &[(i32, i32)])) {
    let Some(y0) = rects.iter().map(|r| r[1]).min() else { return };
    let y1 = rects.iter().map(|r| r[3]).max().unwrap_or(y0);
    let mut spans: Vec<(i32, i32)> = Vec::with_capacity(rects.len());
    let mut merged: Vec<(i32, i32)> = Vec::with_capacity(rects.len());
    for y in y0..y1 {
        spans.clear();
        spans.extend(rects.iter().filter(|r| y >= r[1] && y < r[3]).map(|r| (r[0], r[2])));
        if spans.is_empty() {
            continue;
        }
        spans.sort_unstable();
        merged.clear();
        for &(a, b) in &spans {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        f(y, &merged);
    }
}

// --- cameras in the lockstep input ------------------------------------------------------------

/// A camera position in whole world px (the render camera's centre, rounded).
pub type Camera = (i32, i32);

/// The render camera's position, rounded to whole px (None if not finite).
pub fn camera_px(x: f32, y: f32) -> Option<Camera> {
    let ok = |v: f32| v.is_finite() && v.abs() < 1.0e9;
    (ok(x) && ok(y)).then(|| (round_px(x), round_px(y)))
}

/// Trailer of every input (appended by `no_send_input`, stripped by `no_wait_tick` before Lua
/// sees the input): `[x field][y field][flags]`. Flags: bit 0 = Box2D slow step
/// (`box2d_freeze`), bits 1–2 / 3–4 = the x / y field's kind: 0 = unchanged (no bytes),
/// 1 = i8 delta, 2 = i16 delta, 3 = i32 absolute (little endian). A camera that holds still
/// costs no bytes, and one moving at a steady speed gives identical trailers, so the
/// streams' "same as the previous input" coding keeps working.
const KIND_SAME: u8 = 0;
const KIND_I8: u8 = 1;
const KIND_I16: u8 = 2;
const KIND_ABS: u8 = 3;

fn kind_len(kind: u8) -> usize {
    match kind {
        KIND_I8 => 1,
        KIND_I16 => 2,
        KIND_ABS => 4,
        _ => 0,
    }
}

/// One client's camera stream: each input carries its camera as a delta from the previous
/// input's (the receivers decode every input of the player in frame order).
#[derive(Default)]
pub struct CameraEncoder {
    last: Option<Camera>,
}

impl CameraEncoder {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Forget the reference: the next camera goes out absolute (a gap in the input frames).
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Append the trailer for `cam` (None: unknown, the receivers keep the last one) and the
    /// slow-step flag to `out`.
    pub fn encode(&mut self, cam: Option<Camera>, slow: bool, out: &mut Vec<u8>) {
        let mut flags = slow as u8;
        if let Some(c) = cam {
            let mut field = |shift: u8, v: i32, last: Option<i32>| {
                let kind = match last.map(|l| v.wrapping_sub(l)) {
                    Some(0) => KIND_SAME,
                    Some(d) if i8::try_from(d).is_ok() => {
                        out.push(d as i8 as u8);
                        KIND_I8
                    }
                    Some(d) if i16::try_from(d).is_ok() => {
                        out.extend_from_slice(&(d as i16).to_le_bytes());
                        KIND_I16
                    }
                    _ => {
                        out.extend_from_slice(&v.to_le_bytes());
                        KIND_ABS
                    }
                };
                flags |= kind << shift;
            };
            field(1, c.0, self.last.map(|l| l.0));
            field(3, c.1, self.last.map(|l| l.1));
            self.last = Some(c);
        }
        out.push(flags);
    }
}

/// Strip the trailer from a player's input; returns the slow-step flag and the player's
/// camera after this input (`last`: after its previous input). An empty input (the frames
/// before the input delay) has no trailer; a malformed one leaves the camera unchanged.
pub fn decode_trailer(data: &mut Vec<u8>, last: Option<Camera>) -> (bool, Option<Camera>) {
    let Some(flags) = data.pop() else { return (false, last) };
    let (kx, ky) = ((flags >> 1) & 3, (flags >> 3) & 3);
    let (lx, ly) = (kind_len(kx), kind_len(ky));
    if data.len() < lx + ly {
        return (flags & 1 != 0, last);
    }
    let at = data.len() - lx - ly;
    let field = |kind: u8, b: &[u8], last: Option<i32>| -> Option<i32> {
        match kind {
            KIND_SAME => last,
            KIND_I8 => last.map(|l| l.wrapping_add(b[0] as i8 as i32)),
            KIND_I16 => last.map(|l| l.wrapping_add(i16::from_le_bytes([b[0], b[1]]) as i32)),
            _ => Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        }
    };
    let x = field(kx, &data[at..at + lx], last.map(|c| c.0));
    let y = field(ky, &data[at + lx..], last.map(|c| c.1));
    data.truncate(at);
    let cam = match (x, y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => last,
    };
    (flags & 1 != 0, cam)
}

/// Every player's last camera (index = player id), as decoded from the ticks.
static CAMERAS: Mutex<[Option<Camera>; 128]> = Mutex::new([None; 128]);

/// Decode one tick's inputs (player id, input bytes) in place: strips every trailer, updates
/// the players' cameras; returns whether any player reported a slow Box2D step.
pub fn take_tick_trailers<'a>(inputs: impl Iterator<Item = (u8, &'a mut Vec<u8>)>) -> bool {
    let mut cams = CAMERAS.lock().unwrap_or_else(|p| p.into_inner());
    let mut slow = false;
    for (p, d) in inputs {
        let slot = &mut cams[(p & 0x7f) as usize];
        let (s, cam) = decode_trailer(d, *slot);
        slow |= s;
        *slot = cam;
    }
    slow
}

/// Player `id`'s camera, from the ticks so far (None before its first one).
pub fn camera_of(id: u8) -> Option<Camera> {
    CAMERAS.lock().unwrap_or_else(|p| p.into_inner())[(id & 0x7f) as usize]
}

/// This client's render camera for its next input: the camera position it drew the last
/// frame with, taken before the simulation window moves the camera.
static LOCAL_CAMERA: Mutex<Option<Camera>> = Mutex::new(None);

pub fn set_local_camera(cam: Option<Camera>) {
    *LOCAL_CAMERA.lock().unwrap_or_else(|p| p.into_inner()) = cam;
}

pub fn local_camera() -> Option<Camera> {
    *LOCAL_CAMERA.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_rows_covers_each_pixel_once() {
        let rects = [[0, 0, 10, 4], [5, 2, 20, 6], [30, 3, 32, 5]];
        let mut seen = std::collections::HashSet::new();
        union_rows(&rects, |y, spans| {
            for &(a, b) in spans {
                for x in a..b {
                    assert!(seen.insert((x, y)));
                }
            }
        });
        let want: usize = (0..6)
            .map(|y| (0..40).filter(|&x| rects.iter().any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3])).count())
            .sum();
        assert_eq!(seen.len(), want);
    }

    #[test]
    fn camera_trailer_round_trips() {
        let path: [Option<Camera>; 9] = [
            None,
            Some((355, -107)),
            Some((355, -107)),
            Some((358, -106)),
            Some((358 + 300, -106 - 20_000)),
            None,
            Some((i32::MIN + 5, i32::MAX - 3)),
            Some((-12, 7)),
            Some((-12, 7)),
        ];
        let mut enc = CameraEncoder::default();
        let mut last = None;
        let mut lens = Vec::new();
        for (i, &cam) in path.iter().enumerate() {
            let payload = vec![0xAB; i];
            let mut bytes = payload.clone();
            enc.encode(cam, i % 2 == 1, &mut bytes);
            lens.push(bytes.len() - payload.len());
            let (slow, got) = decode_trailer(&mut bytes, last);
            assert_eq!(bytes, payload);
            assert_eq!(slow, i % 2 == 1);
            let want = cam.or(last);
            assert_eq!(got, want, "step {i}");
            last = got;
        }
        // Unknown and unchanged cameras cost the flags byte only, small moves one byte per axis.
        assert_eq!(lens, vec![1, 9, 1, 3, 5, 1, 9, 9, 1]);
    }

    #[test]
    fn camera_trailer_tolerates_empty_and_short_inputs() {
        let mut empty = Vec::new();
        assert_eq!(decode_trailer(&mut empty, Some((1, 2))), (false, Some((1, 2))));
        // Flags announcing an absolute x with no bytes in front: camera unchanged.
        let mut short = vec![KIND_ABS << 1 | 1];
        assert_eq!(decode_trailer(&mut short, Some((1, 2))), (true, Some((1, 2))));
        assert!(short.is_empty());
    }
}
